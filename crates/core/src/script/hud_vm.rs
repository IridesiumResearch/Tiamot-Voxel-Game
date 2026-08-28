// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The client-side HUD runtime: tier 2 of Task 14.
//!
//! # A different sandbox from the mod host, on purpose
//!
//! [`super::mlua_vm`] builds a mod's environment by copying the real globals
//! and REMOVING a deny-list, and says why: a mod runs on a server its operator
//! chose, and the failure mode worth optimising for is "a mod cannot use
//! `string.format`".
//!
//! A pushed client script is not that. It arrives from a server a player
//! connected to and runs on the player's own machine, so the failure mode worth
//! optimising for is the other one entirely — and this environment is an
//! **allow-list**. Nothing is in it unless it is named in [`ALLOWED_GLOBALS`],
//! which means a future Lua version cannot add a capability into the sandbox by
//! existing. There is no `load` at all, not even the text-only one mods get:
//! charter rule 10 bans binary chunks, and a HUD has no honest use for the rest.
//!
//! # Budgeted per FRAME, not per call
//!
//! A mod's budget is per callback because a mod's callbacks are occasional. A
//! HUD script runs sixty times a second, so the same number would let it spend
//! sixty times as much. [`HudLimits::instructions_per_frame`] is smaller by
//! three orders of magnitude and is the whole of what stands between a pushed
//! script and an unresponsive client.
//!
//! # What happens when a script goes over
//!
//! Not a crash, and not a silent stall. The script's drawing for that frame is
//! rewound — a half-drawn hotbar is worse than none — the player is told which
//! MOD did it, and the script sits out [`HudLimits::cooldown_frames`]. Enough
//! strikes and it is disabled for the session. The engine's own HUD is drawn by
//! the client and is not on this path at all, so it is unaffected by any of it.
//!
//! The report is emitted on the frames where something CHANGED, never on every
//! frame: a warning printed sixty times a second is a warning nobody can read,
//! and it would cost more than the script it was complaining about.

use mlua::{Lua, Table, Value};

use crate::hud::{Anchor, Builtin, Command, Fill, Frame, State};
use crate::material::MaterialId;
use crate::script::ScriptError;
use crate::ui::Colour;

/// Everything a HUD script may name. Nothing else is in its environment.
///
/// An allow-list rather than the mod host's deny-list — see the module docs.
/// Names absent from a given backend are skipped rather than faulted: `bit` is
/// `LuaJIT`'s, `bit32` and `utf8` arrived in later standard Luas, and a script
/// that wants one can check for it.
const ALLOWED_GLOBALS: [&str; 21] = [
    "assert",
    "bit",
    "bit32",
    "error",
    "getmetatable",
    "ipairs",
    "math",
    "next",
    "pairs",
    "pcall",
    "rawequal",
    "rawget",
    "rawlen",
    "rawset",
    "select",
    "setmetatable",
    "string",
    "table",
    "tonumber",
    "tostring",
    "type",
];

/// What bounds a HUD script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HudLimits {
    /// Ceiling on instructions executed in one script's draw callback, per
    /// frame.
    pub instructions_per_frame: u32,
    /// Ceiling on total VM allocation, in bytes.
    pub memory_bytes: usize,
    /// What one frame's draw list may not exceed.
    pub frame: crate::hud::Limits,
    /// Most scripts that may be loaded at once.
    pub scripts: usize,
    /// How many frames a script sits out after going over.
    pub cooldown_frames: u64,
    /// How many times it may go over before it is disabled for the session.
    pub strikes: u32,
}

impl Default for HudLimits {
    fn default() -> Self {
        Self {
            // Roughly a tenth of a millisecond of interpreted Lua. A HUD reads
            // a dozen values and emits a dozen commands; anything approaching
            // this ceiling is computing something that does not belong here.
            instructions_per_frame: 200_000,
            // Smaller than a mod's 256 MiB: a HUD holds a few strings and a
            // table of slots, and nothing legitimate here needs a lookup table.
            memory_bytes: 32 * 1024 * 1024,
            frame: crate::hud::Limits::default(),
            // A server pushing more than this many HUD scripts is doing
            // something other than drawing a HUD.
            scripts: 8,
            // A fifth of a second at 60 fps. Long enough that a runaway script
            // costs a fraction of the frames rather than all of them, short
            // enough that a script over budget once recovers unnoticed.
            cooldown_frames: 12,
            strikes: 5,
        }
    }
}

/// Something a HUD script did that the player should be told about.
///
/// Mod-attributed, because "the HUD is broken" is unactionable and "`core_ui`'s
/// HUD script went over its frame budget" is a bug report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fault {
    /// Which mod's script.
    pub mod_id: String,
    /// What happened, in a sentence a player can repeat.
    pub message: String,
    /// Whether it is out for the rest of the session, rather than cooling off.
    pub disabled: bool,
}

/// Where a script stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Health {
    /// Drawing normally.
    Running,
    /// Sitting out until this frame number, having gone over.
    Cooling {
        /// The first frame it may draw again.
        until: u64,
    },
    /// Out for the session.
    Disabled,
}

/// One loaded script.
struct Script {
    mod_id: String,
    /// Its sandbox, which also holds the callback it registered.
    env: Table,
    health: Health,
    strikes: u32,
}

/// Key under which a script's draw callback lives in its own environment.
///
/// In the environment rather than a shared registry, because unlike a mod hook
/// there is nobody to enumerate: a HUD callback belongs to exactly one script
/// and is called by exactly one loop.
const CALLBACK: &str = "__tiamot_on_draw";

/// The runtime that runs pushed client scripts and collects what they draw.
pub struct HudVm {
    lua: Lua,
    scripts: Vec<Script>,
    limits: HudLimits,
    frames: u64,
}

impl HudVm {
    /// Builds an empty runtime.
    ///
    /// # Errors
    ///
    /// [`ScriptError::Vm`] if the VM cannot be created or bounded.
    pub fn new(limits: HudLimits) -> Result<Self, ScriptError> {
        let lua = Lua::new();

        // LuaJIT has no allocator hook, so a memory ceiling is unavailable
        // there — the same honest gap `docs/scripting-vm.md` records for mods.
        #[cfg(not(feature = "vm-luajit"))]
        lua.set_memory_limit(limits.memory_bytes)
            .map_err(|err| ScriptError::Vm {
                backend: "hud",
                detail: err.to_string(),
            })?;

        lua.set_app_data(Frame::new(limits.frame));

        Ok(Self {
            lua,
            scripts: Vec::new(),
            limits,
            frames: 0,
        })
    }

    /// Loads one mod's HUD script.
    ///
    /// The source runs once, here, with a budget on it: registering a callback
    /// is the only thing a HUD script's top level should do, and a top level
    /// that loops forever must not hang the client before a frame is ever
    /// drawn.
    ///
    /// # Errors
    ///
    /// [`ScriptError::Load`] for a script that will not parse or run, and for
    /// one past [`HudLimits::scripts`].
    pub fn load(&mut self, mod_id: &str, source: &str) -> Result<(), ScriptError> {
        if self.scripts.len() >= self.limits.scripts {
            return Err(ScriptError::Load {
                mod_id: mod_id.to_owned(),
                detail: format!(
                    "this server has already pushed {} HUD scripts, which is all a client will run",
                    self.limits.scripts
                ),
            });
        }

        let env = self.build_environment(mod_id)?;
        let chunk = self
            .lua
            .load(source)
            .set_name(format!("@{mod_id}/hud.lua"))
            .set_environment(env.clone());

        super::budget::arm(&self.lua, self.limits.instructions_per_frame)
            .map_err(|err| Self::load_error(mod_id, &err))?;
        let outcome = chunk.exec();
        super::budget::disarm(&self.lua);
        outcome.map_err(|err| Self::load_error(mod_id, &err))?;

        self.scripts.push(Script {
            mod_id: mod_id.to_owned(),
            env,
            health: Health::Running,
            strikes: 0,
        });
        Ok(())
    }

    /// Runs every loaded script's draw callback and collects what they drew.
    ///
    /// Returns only what CHANGED — a script entering cooldown or being
    /// disabled. A script quietly sitting out reports nothing, because a
    /// warning repeated sixty times a second is not a warning.
    pub fn draw(&mut self, state: &State) -> Vec<Fault> {
        self.frames += 1;
        if let Some(mut frame) = self.lua.app_data_mut::<Frame>() {
            frame.clear();
        }

        let mut faults = Vec::new();
        for index in 0..self.scripts.len() {
            match self.scripts[index].health {
                Health::Disabled => continue,
                Health::Cooling { until } if self.frames < until => continue,
                Health::Cooling { .. } | Health::Running => {}
            }
            if let Some(fault) = self.draw_one(index, state) {
                faults.push(fault);
            }
        }
        faults
    }

    /// Runs one script, rewinding whatever it drew if it faults.
    fn draw_one(&mut self, index: usize, state: &State) -> Option<Fault> {
        let mark = self
            .lua
            .app_data_ref::<Frame>()
            .map(|frame| frame.checkpoint())?;

        let outcome = self.call_draw(index, state);
        let Err(err) = outcome else {
            self.scripts[index].health = Health::Running;
            return None;
        };

        // Everything this script put on the frame goes, not just what it was
        // doing when it faulted. A hotbar missing its last three slots reads as
        // lost items.
        if let Some(mut frame) = self.lua.app_data_mut::<Frame>() {
            frame.rewind(mark);
        }

        let over_budget = err.to_string().contains(super::budget::MARKER);
        let script = &mut self.scripts[index];
        script.strikes += 1;
        let disabled = script.strikes >= self.limits.strikes;
        script.health = if disabled {
            Health::Disabled
        } else {
            Health::Cooling {
                until: self.frames + self.limits.cooldown_frames,
            }
        };

        let what = if over_budget {
            format!(
                "went over its {} instruction frame budget",
                self.limits.instructions_per_frame
            )
        } else {
            format!("failed: {}", first_line(&err.to_string()))
        };
        let outcome = if disabled {
            "it is switched off for this session".to_owned()
        } else {
            format!("it will sit out {} frames", self.limits.cooldown_frames)
        };
        Some(Fault {
            mod_id: script.mod_id.clone(),
            message: format!("{}'s HUD script {what}; {outcome}", script.mod_id),
            disabled,
        })
    }

    /// Calls one script's registered callback under a fresh budget.
    fn call_draw(&self, index: usize, state: &State) -> Result<(), mlua::Error> {
        let callback: Value = self.scripts[index].env.get(CALLBACK)?;
        let Value::Function(callback) = callback else {
            // A script that never called `hud.on_draw` draws nothing. Not a
            // fault: a mod may push a script that only reacts to something.
            return Ok(());
        };
        let table = self.state_table(state, &self.scripts[index].mod_id)?;

        super::budget::arm(&self.lua, self.limits.instructions_per_frame)?;
        let outcome = callback.call::<()>(table);
        super::budget::disarm(&self.lua);
        outcome
    }

    /// What this frame drew, for a renderer.
    ///
    /// Handed to a closure rather than returned, because the frame lives inside
    /// the VM and cloning five hundred commands sixty times a second to avoid
    /// that would cost more than drawing them.
    pub fn with_frame<T>(&self, visit: impl FnOnce(&Frame) -> T) -> Option<T> {
        self.lua.app_data_ref::<Frame>().map(|frame| visit(&frame))
    }

    /// Which mods have a script loaded, in load order.
    pub fn loaded(&self) -> impl Iterator<Item = &str> {
        self.scripts.iter().map(|script| script.mod_id.as_str())
    }

    /// Whether a mod's script is switched off for the session.
    #[must_use]
    pub fn is_disabled(&self, mod_id: &str) -> bool {
        self.scripts
            .iter()
            .any(|script| script.mod_id == mod_id && script.health == Health::Disabled)
    }

    fn load_error(mod_id: &str, err: &mlua::Error) -> ScriptError {
        ScriptError::Load {
            mod_id: mod_id.to_owned(),
            detail: err.to_string(),
        }
    }
}

/// The first line of an error, which is the part with the message in it.
fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or(text).trim()
}

/// Reading a Lua number as a whole virtual pixel.
///
/// Lua has one number type, so everything arrives as an `f64` and the three
/// awkward values have to be decided here rather than three layers down in a
/// renderer:
///
/// - **`NaN`** becomes the default. It is not a quantity, so there is no
///   sensible bound to clamp it to.
/// - **±infinity** saturates to the bound, because Rust's float-to-int cast
///   saturates rather than wrapping. A script writing `filled / total` with a
///   zero total meant "all of it", and that is what it gets — the same answer
///   it would get from writing `9999`, which is the point: a value out of range
///   should not depend on HOW it got out of range.
/// - **Anything else** is truncated and clamped.
fn whole(value: Option<f64>, default: i64, low: i64, high: i64) -> i64 {
    let Some(value) = value else {
        return default;
    };
    if value.is_nan() {
        return default;
    }
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the cast saturates and the clamp bounds it"
    )]
    let value = value as i64;
    value.clamp(low, high)
}

/// An `i16` field: an offset from an anchor.
fn offset(table: &Table, key: &str) -> mlua::Result<i16> {
    let raw = table.get::<Option<f64>>(key)?;
    #[expect(
        clippy::cast_possible_truncation,
        reason = "clamped to i16 range by `whole`"
    )]
    Ok(whole(raw, 0, i64::from(i16::MIN), i64::from(i16::MAX)) as i16)
}

/// A `u16` field: an extent in virtual pixels.
fn extent(table: &Table, key: &str, default: u16) -> mlua::Result<u16> {
    let raw = table.get::<Option<f64>>(key)?;
    #[expect(
        clippy::cast_possible_truncation,
        reason = "clamped to u16 range by `whole`"
    )]
    Ok(whole(raw, i64::from(default), 0, i64::from(u16::MAX)) as u16)
}

/// A colour field, as four bytes. Absent means the default.
fn colour(table: &Table, key: &str, default: Colour) -> mlua::Result<Colour> {
    let Some(list) = table.get::<Option<Table>>(key)? else {
        return Ok(default);
    };
    let mut out = default;
    for (index, slot) in out.iter_mut().enumerate() {
        // 1-based, because this is Lua and `{255, 255, 255}` is what an author
        // will write. A missing alpha means opaque, which is what a script that
        // wrote three numbers meant.
        let value = list.get::<Option<f64>>(index + 1)?;
        #[expect(
            clippy::cast_possible_truncation,
            reason = "clamped to u8 range by `whole`"
        )]
        let byte = whole(value, i64::from(*slot), 0, 255) as u8;
        *slot = byte;
    }
    Ok(out)
}

/// The anchor a table named, defaulting to the top-left corner.
fn anchor(table: &Table) -> mlua::Result<Anchor> {
    let Some(name) = table.get::<Option<String>>("anchor")? else {
        return Ok(Anchor::TopLeft);
    };
    // Both spellings of the middle. The engine writes "centre" and a mod author
    // may well not; refusing one of them would be a papercut with no upside.
    match name.as_str() {
        "top_left" => Ok(Anchor::TopLeft),
        "top" => Ok(Anchor::Top),
        "top_right" => Ok(Anchor::TopRight),
        "left" => Ok(Anchor::Left),
        "centre" | "center" => Ok(Anchor::Centre),
        "right" => Ok(Anchor::Right),
        "bottom_left" => Ok(Anchor::BottomLeft),
        "bottom" => Ok(Anchor::Bottom),
        "bottom_right" => Ok(Anchor::BottomRight),
        other => Err(mlua::Error::external(format!(
            "`{other}` is not an anchor: expected one of top_left, top, top_right, left, centre, \
             right, bottom_left, bottom, bottom_right"
        ))),
    }
}

/// Adds a command to this frame, or turns the refusal into a Lua error.
///
/// The script SEES the refusal, which is the whole point — see
/// [`crate::hud::HudError`].
fn emit(lua: &Lua, command: Command) -> mlua::Result<()> {
    let Some(mut frame) = lua.app_data_mut::<Frame>() else {
        return Err(mlua::Error::external(
            "the HUD frame is not available; this is an engine fault, not a script one",
        ));
    };
    frame.push(command).map_err(mlua::Error::external)
}

impl HudVm {
    /// Builds one script's sandbox.
    ///
    /// **An allow-list**, unlike the mod host's deny-list — see the module docs
    /// for why the threat models differ. A name the backend does not have is
    /// skipped, so this same list works on all three.
    fn build_environment(&self, mod_id: &str) -> Result<Table, ScriptError> {
        let env = self
            .lua
            .create_table()
            .map_err(|err| Self::load_error(mod_id, &err))?;
        let globals = self.lua.globals();
        for name in ALLOWED_GLOBALS {
            let value: Value = globals
                .get(name)
                .map_err(|err| Self::load_error(mod_id, &err))?;
            if value.is_nil() {
                continue;
            }
            env.set(name, value)
                .map_err(|err| Self::load_error(mod_id, &err))?;
        }

        // `_G` points at the sandbox. A script's own globals have to go
        // somewhere, and the real table is not it.
        env.set("_G", env.clone())
            .map_err(|err| Self::load_error(mod_id, &err))?;

        let hud = self
            .build_hud_table(mod_id, &env)
            .map_err(|err| Self::load_error(mod_id, &err))?;
        env.set("hud", hud)
            .map_err(|err| Self::load_error(mod_id, &err))?;
        Ok(env)
    }

    /// The five commands that put something on the screen.
    ///
    /// Split out because `build_hud_table` was over clippy's 100-line ceiling —
    /// which is the fourth time in this task that appending an arm to a
    /// well-named function was the wrong move.
    fn install_draw_commands(&self, hud: &Table) -> mlua::Result<()> {
        hud.set(
            "text",
            self.lua.create_function(|lua, spec: Table| {
                emit(
                    lua,
                    Command::Text {
                        anchor: anchor(&spec)?,
                        x: offset(&spec, "x")?,
                        y: offset(&spec, "y")?,
                        text: spec.get::<Option<String>>("text")?.unwrap_or_default(),
                        size: extent(&spec, "size", 24)?,
                        colour: colour(&spec, "colour", [255, 255, 255, 255])?,
                    },
                )
            })?,
        )?;

        hud.set(
            "rect",
            self.lua.create_function(|lua, spec: Table| {
                emit(
                    lua,
                    Command::Rect {
                        anchor: anchor(&spec)?,
                        x: offset(&spec, "x")?,
                        y: offset(&spec, "y")?,
                        w: extent(&spec, "w", 0)?,
                        h: extent(&spec, "h", 0)?,
                        colour: colour(&spec, "colour", [0, 0, 0, 160])?,
                    },
                )
            })?,
        )?;

        hud.set(
            "image",
            self.lua.create_function(|lua, spec: Table| {
                let hash = spec.get::<Option<String>>("hash")?.unwrap_or_default();
                emit(
                    lua,
                    Command::Image {
                        anchor: anchor(&spec)?,
                        x: offset(&spec, "x")?,
                        y: offset(&spec, "y")?,
                        w: extent(&spec, "w", 0)?,
                        h: extent(&spec, "h", 0)?,
                        hash: parse_hash(&hash)?,
                    },
                )
            })?,
        )?;

        hud.set(
            "bar",
            self.lua.create_function(|lua, spec: Table| {
                let fill = spec.get::<Option<f64>>("fill")?.unwrap_or(0.0);
                emit(
                    lua,
                    Command::Bar {
                        anchor: anchor(&spec)?,
                        x: offset(&spec, "x")?,
                        y: offset(&spec, "y")?,
                        w: extent(&spec, "w", 0)?,
                        h: extent(&spec, "h", 0)?,
                        // Per-mille inside, a 0..1 fraction outside: a script
                        // writing `health / max` should not have to know how
                        // the engine stores it. `Fill` clamps on top of
                        // `whole`, so no arithmetic a script can do produces a
                        // bar longer than its own rectangle.
                        #[expect(
                            clippy::cast_possible_truncation,
                            reason = "clamped to 0..=1000 by `whole`"
                        )]
                        fill: Fill::per_mille(whole(Some(fill * 1000.0), 0, 0, 1000) as i32),
                        colour: colour(&spec, "colour", [255, 255, 255, 255])?,
                        background: colour(&spec, "background", [0, 0, 0, 160])?,
                    },
                )
            })?,
        )?;

        self.install_icon(hud)?;
        Ok(())
    }

    /// The `hud.icon` command, on its own because the draw table is at
    /// clippy's line ceiling and appending to it is what put it there.
    fn install_icon(&self, hud: &Table) -> mlua::Result<()> {
        hud.set(
            "icon",
            self.lua.create_function(|lua, spec: Table| {
                let raw = spec.get::<Option<f64>>("material")?;
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "clamped to u16 range by `whole`"
                )]
                let material = MaterialId(whole(raw, 0, 0, i64::from(u16::MAX)) as u16);
                // Masked to the twenty-seven bits a shape has. A script that
                // passed a larger number gets the cells that exist rather than
                // an error: the same saturating rule every other field here
                // follows, so a value out of range cannot depend on HOW it got
                // out of range.
                #[expect(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "clamped to the 27-bit mask by `whole`"
                )]
                let shape = whole(
                    spec.get::<Option<f64>>("shape")?,
                    0,
                    0,
                    i64::from(crate::inventory::Shape::ALL),
                ) as u32;
                emit(
                    lua,
                    Command::Icon {
                        anchor: anchor(&spec)?,
                        x: offset(&spec, "x")?,
                        y: offset(&spec, "y")?,
                        size: extent(&spec, "size", 48)?,
                        material,
                        shape,
                    },
                )
            })?,
        )?;

        Ok(())
    }

    /// The `hud` table: everything a client script may do.
    fn build_hud_table(&self, mod_id: &str, env: &Table) -> mlua::Result<Table> {
        let hud = self.lua.create_table()?;

        let target = env.clone();
        hud.set(
            "on_draw",
            self.lua
                .create_function(move |_, callback: mlua::Function| {
                    // Last registration wins rather than accumulating. A script
                    // reloaded — or one that registers in a loop — should not
                    // end up drawing its HUD n times.
                    target.set(CALLBACK, callback)
                })?,
        )?;

        self.install_draw_commands(&hud)?;
        hud.set(
            "hide_builtin",
            self.lua.create_function(|lua, name: String| {
                let builtin = Builtin::parse(&name).ok_or_else(|| {
                    let known: Vec<&str> = Builtin::all().iter().map(|b| b.name()).collect();
                    mlua::Error::external(format!(
                        "`{name}` is not an engine HUD element: the engine draws {}. Chat and the \
                         settings screen are not on this list and cannot be hidden — moderation \
                         and rebinding have to work whatever a server pushes.",
                        known.join(", ")
                    ))
                })?;
                let Some(mut frame) = lua.app_data_mut::<Frame>() else {
                    return Err(mlua::Error::external("the HUD frame is not available"));
                };
                frame.hide(builtin);
                Ok(())
            })?,
        )?;

        hud.set(
            "screen",
            self.lua.create_function(|_, ()| {
                // Height only. A script asking for the width would be asking a
                // question whose answer changes per window; anchors are how it
                // gets what it actually wanted.
                Ok(u32::from(crate::hud::VIRTUAL_HEIGHT))
            })?,
        )?;

        let owner = mod_id.to_owned();
        hud.set(
            "log",
            self.lua.create_function(move |_, message: String| {
                // Attributed, and it costs the script instructions like
                // anything else — so a log in a hot loop hits the frame budget
                // rather than the player's terminal.
                tracing::info!(mod_id = %owner, "{message}");
                Ok(())
            })?,
        )?;

        Ok(hud)
    }

    /// Builds the read-only state table handed to a draw callback.
    fn state_table(&self, state: &State, mod_id: &str) -> mlua::Result<Table> {
        let table = self.lua.create_table()?;
        // **This mod's values and no other's.** The state carries every mod's,
        // keyed by id, and a script is handed the entry for the mod that
        // pushed it — so `state.values` needs no namespacing and there is
        // nowhere in the surface for a script to name somebody else's. The
        // same isolation `game.storage` has on the other side.
        let values = self.lua.create_table()?;
        if let Some(mine) = state.values.get(mod_id) {
            for (key, value) in mine {
                match value {
                    crate::hud::Value::Number(number) => values.set(key.as_str(), *number)?,
                    crate::hud::Value::Text(text) => values.set(key.as_str(), text.as_str())?,
                    crate::hud::Value::Flag(flag) => values.set(key.as_str(), *flag)?,
                }
            }
        }
        table.set("values", values)?;
        table.set("x", state.position[0])?;
        table.set("y", state.position[1])?;
        table.set("z", state.position[2])?;
        table.set("yaw", state.yaw)?;
        table.set("pitch", state.pitch)?;
        table.set("time_of_day", state.time_of_day)?;
        // 1-based, because every other index a Lua author touches is.
        table.set("selected", state.selected + 1)?;

        let carried = self.lua.create_table()?;
        // **An empty slot is a hole, not a missing entry.** A hotbar is a row
        // of places, so slot four has to be readable as slot four whether or
        // not anything is in it — a script that compacted the list would draw
        // a player's fourth stack under their third key.
        for (index, entry) in state.carried.iter().enumerate() {
            let Some(entry) = entry else { continue };
            let slot = self.lua.create_table()?;
            slot.set("material", entry.material.0)?;
            slot.set("name", entry.name.as_str())?;
            slot.set("units", entry.units)?;
            // Charter rule 5's display, done once by the engine. Every HUD that
            // shows a count shows the same count.
            let (blocks, nodes) = entry.display();
            slot.set("blocks", blocks)?;
            slot.set("nodes", nodes)?;
            slot.set("shape", (entry.shape != 0).then_some(entry.shape))?;
            // Items, for a cut. `nil` for loose material, where blocks and
            // spare nodes is the display and a count means nothing.
            slot.set("count", entry.count())?;
            carried.set(index + 1, slot)?;
        }
        // How many places there are, which `#carried` cannot answer once there
        // are holes in it.
        table.set("slots", state.carried.len())?;
        table.set("carried", carried)?;

        // The off-hand, separately, because a HUD draws it somewhere else.
        let offhand = match &state.offhand {
            Some(entry) => {
                let slot = self.lua.create_table()?;
                slot.set("material", entry.material.0)?;
                slot.set("name", entry.name.as_str())?;
                slot.set("units", entry.units)?;
                let (blocks, nodes) = entry.display();
                slot.set("blocks", blocks)?;
                slot.set("nodes", nodes)?;
                slot.set("shape", (entry.shape != 0).then_some(entry.shape))?;
                // Items, for a cut. `nil` for loose material, where blocks and
                // spare nodes is the display and a count means nothing.
                slot.set("count", entry.count())?;
                Some(slot)
            }
            None => None,
        };
        table.set("offhand", offhand)?;

        if let Some(look) = &state.looking_at {
            let hit = self.lua.create_table()?;
            hit.set("x", look.cell[0])?;
            hit.set("y", look.cell[1])?;
            hit.set("z", look.cell[2])?;
            hit.set("material", look.material.0)?;
            hit.set("name", look.name.as_str())?;
            table.set("looking_at", hit)?;
        }

        if let Some(dig) = state.dig {
            table.set("dig", dig.fraction())?;
        }

        if let Some(tool) = &state.tool {
            let held = self.lua.create_table()?;
            held.set("id", tool.id.as_str())?;
            held.set("name", tool.name.as_str())?;
            held.set("brush", tool.brush.as_str())?;
            table.set("tool", held)?;
        }

        Ok(table)
    }
}

/// Parses the hex content hash a script wrote.
fn parse_hash(text: &str) -> mlua::Result<crate::proto::ContentHash> {
    let mut hash = [0u8; 32];
    if text.len() != 64 {
        return Err(mlua::Error::external(format!(
            "`{text}` is not a content hash: expected 64 hex characters, got {}",
            text.len()
        )));
    }
    for (index, slot) in hash.iter_mut().enumerate() {
        let pair = text
            .get(index * 2..index * 2 + 2)
            .ok_or_else(|| mlua::Error::external("a content hash must be plain ASCII hex"))?;
        *slot = u8::from_str_radix(pair, 16)
            .map_err(|_| mlua::Error::external(format!("`{pair}` is not hex")))?;
    }
    Ok(hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hud::Carried;

    fn vm() -> HudVm {
        HudVm::new(HudLimits::default()).expect("hud vm")
    }

    /// A VM whose budget is small enough that a loop trips it quickly.
    fn tight_vm() -> HudVm {
        HudVm::new(HudLimits {
            instructions_per_frame: 8_192,
            cooldown_frames: 3,
            strikes: 3,
            ..HudLimits::default()
        })
        .expect("hud vm")
    }

    fn state() -> State {
        State {
            position: [1.0, 2.0, 3.0],
            selected: 0,
            carried: vec![Some(Carried {
                material: MaterialId(7),
                name: "core_blocks:white".to_owned(),
                // Charter rule 5's example, exactly: forty units is one block
                // and thirteen spare nodes.
                units: 40,
                shape: 0,
            })],
            ..State::default()
        }
    }

    fn commands(vm: &HudVm) -> Vec<Command> {
        vm.with_frame(|frame| frame.commands().to_vec())
            .expect("frame")
    }

    #[test]
    fn a_script_draws_what_it_asked_for_in_the_order_it_asked() {
        let mut vm = vm();
        vm.load(
            "core_ui",
            r#"
hud.on_draw(function(state)
    hud.rect{ anchor = "bottom", x = -200, y = 80, w = 400, h = 64 }
    hud.text{ anchor = "bottom", x = 0, y = 60, text = "slot " .. state.selected }
end)
"#,
        )
        .expect("load");

        assert!(vm.draw(&state()).is_empty(), "a good script has no faults");
        let drawn = commands(&vm);
        assert_eq!(drawn.len(), 2);
        assert!(
            matches!(drawn[0], Command::Rect { .. }),
            "the panel was said first, so it is behind"
        );
        let Command::Text { text, anchor, .. } = &drawn[1] else {
            panic!("expected text second");
        };
        assert_eq!(text, "slot 1", "indices handed to Lua are 1-based");
        assert_eq!(*anchor, Anchor::Bottom);
    }

    #[test]
    fn the_reference_hud_counts_a_cut_and_measures_loose_material() {
        // **The real `game/core_ui/hud.lua`, not a fixture.** What a hotbar
        // says a stack is was reported wrong from the window twice over: a cut
        // was labelled with its UNITS, so one thirteen-cell stair read `+13`,
        // and it was drawn with the material's flat tile, so stairs looked like
        // stone. Both are decisions in that file, and a fixture that repeated
        // them would prove nothing about the mod that ships.
        // One item of a thirteen-cell shape, which is thirteen units.
        const CUT: u32 = 0b1_1010_1010_1010;

        let source = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../game/core_ui/hud.lua"
        ))
        .expect("the reference HUD script");

        let mut vm = vm();
        vm.load("core_ui", &source).expect("load");

        let mut cut_state = state();
        cut_state.carried = vec![Some(Carried {
            material: MaterialId(7),
            name: "core_blocks:white".to_owned(),
            units: CUT.count_ones(),
            shape: CUT,
        })];
        assert!(vm.draw(&cut_state).is_empty(), "the reference HUD faulted");
        let drawn = commands(&vm);

        let labels: Vec<&str> = drawn
            .iter()
            .filter_map(|command| match command {
                Command::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            labels.contains(&"1"),
            "one stair should be labelled `1`, and the labels were {labels:?}"
        );
        assert!(
            !labels.iter().any(|label| label.contains("13")),
            "the cut was labelled with its units: {labels:?}"
        );
        assert!(
            drawn.iter().any(|command| matches!(
                command,
                Command::Icon { shape, .. } if *shape == CUT
            )),
            "the hotbar drew the material rather than the cut"
        );

        // And loose material is unchanged: charter rule 5's blocks and spare
        // nodes, with no shape for the icon to draw.
        assert!(vm.draw(&state()).is_empty());
        let drawn = commands(&vm);
        let labels: Vec<&str> = drawn
            .iter()
            .filter_map(|command| match command {
                Command::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            labels.contains(&"1+13"),
            "forty loose units should still read `1+13`, and the labels were {labels:?}"
        );
        assert!(
            drawn.iter().any(|command| matches!(
                command,
                Command::Icon { shape, .. } if *shape == 0
            )),
            "loose material asked for a shape"
        );
    }

    #[test]
    fn a_script_sees_its_own_mods_values_and_no_others() {
        // **The isolation is the whole design.** A HUD script is handed the
        // values its OWN mod set, so `state.values` needs no namespacing and
        // there is nowhere in the surface for one mod to read another's — the
        // same shape `game.storage` has on the server side.
        let mut vm = vm();
        for mod_id in ["core_health", "core_magic"] {
            vm.load(
                mod_id,
                r#"
hud.on_draw(function(state)
    local shown = {}
    for name, value in pairs(state.values) do
        shown[#shown + 1] = name .. "=" .. tostring(value)
    end
    table.sort(shown)
    hud.text{ anchor = "top", x = 0, y = 0, text = table.concat(shown, ",") }
end)
"#,
            )
            .expect("load");
        }

        let mut state = state();
        state.values.insert(
            "core_health".to_owned(),
            [
                ("health".to_owned(), crate::hud::Value::Number(12.0)),
                ("poisoned".to_owned(), crate::hud::Value::Flag(true)),
            ]
            .into_iter()
            .collect(),
        );
        state.values.insert(
            "core_magic".to_owned(),
            [("school".to_owned(), crate::hud::Value::Text("fire".into()))]
                .into_iter()
                .collect(),
        );

        assert!(vm.draw(&state).is_empty(), "a good script has no faults");
        let lines: Vec<String> = commands(&vm)
            .into_iter()
            .filter_map(|command| match command {
                Command::Text { text, .. } => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(
            lines,
            [
                "health=12.0,poisoned=true".to_owned(),
                "school=fire".to_owned(),
            ],
            "a script saw values that were not its mod's"
        );
    }

    #[test]
    fn a_script_whose_mod_sent_nothing_gets_an_empty_table_and_not_nil() {
        // A HUD drawn before its mod has said anything, which is every HUD for
        // the first frame or two. `state.values.health` must be nil rather
        // than an error about indexing nil — the difference between a bar that
        // is not there yet and a script that is disabled for a session.
        let mut vm = vm();
        vm.load(
            "core_health",
            r#"
hud.on_draw(function(state)
    if state.values.health == nil then
        hud.text{ anchor = "top", x = 0, y = 0, text = "no values yet" }
    end
end)
"#,
        )
        .expect("load");
        assert!(vm.draw(&state()).is_empty(), "a good script has no faults");
        assert_eq!(commands(&vm).len(), 1, "the script did not run cleanly");
    }

    #[test]
    fn a_frame_is_rebuilt_from_nothing_every_time() {
        // Immediate mode: what was drawn last frame is gone, without the script
        // having to undo it.
        let mut vm = vm();
        vm.load(
            "core_ui",
            "hud.on_draw(function() hud.text{ text = 'hi' } end)",
        )
        .expect("load");
        vm.draw(&state());
        vm.draw(&state());
        assert_eq!(commands(&vm).len(), 1, "not two");
    }

    #[test]
    fn the_sandbox_has_no_filesystem_no_network_and_no_way_to_load_code() {
        // Charter rule 10's hard sandbox, name by name. An allow-list means a
        // future Lua cannot add a capability in here by existing — but the
        // named ones are what a reviewer will look for, so they are asserted.
        let mut vm = vm();
        vm.load(
            "hostile",
            r#"
local reachable = {}
for _, name in ipairs({
    "os", "io", "package", "require", "dofile", "loadfile", "load", "loadstring",
    "debug", "coroutine", "collectgarbage", "print", "newproxy", "ffi", "arg",
}) do
    if _G[name] ~= nil then reachable[#reachable + 1] = name end
end
if #reachable > 0 then
    error("reachable: " .. table.concat(reachable, ", "))
end
"#,
        )
        .expect("nothing on that list should be reachable");
    }

    #[test]
    fn a_script_cannot_reach_the_real_globals_through_underscore_g() {
        let mut vm = vm();
        vm.load("hostile", "_G.smuggled = 1")
            .expect("writing to its own globals is fine");
        vm.load(
            "other",
            "if _G.smuggled ~= nil then error('shared globals') end",
        )
        .expect("a second script must not see the first's globals");
    }

    #[test]
    fn a_script_over_the_frame_budget_is_rewound_throttled_and_attributed() {
        // The criterion: throttled, with a user-visible MOD-ATTRIBUTED warning,
        // and the engine's own HUD untouched — which it is by construction,
        // since the client draws that and it is not on this path.
        let mut vm = tight_vm();
        vm.load(
            "greedy",
            r#"
hud.on_draw(function()
    hud.text{ text = "half a hotbar" }
    while true do end
end)
"#,
        )
        .expect("load");

        let faults = vm.draw(&state());
        assert_eq!(faults.len(), 1);
        assert_eq!(faults[0].mod_id, "greedy", "the player is told WHICH mod");
        assert!(
            faults[0].message.contains("frame budget"),
            "and what it did: {}",
            faults[0].message
        );
        assert!(!faults[0].disabled, "one strike is a cooldown, not a ban");
        assert!(
            commands(&vm).is_empty(),
            "the text it managed before stalling is rewound — a half-drawn HUD \
             is worse than none"
        );
    }

    #[test]
    fn a_throttled_script_is_silent_while_it_sits_out() {
        // A warning printed sixty times a second is not a warning, and would
        // cost more than the script it complains about.
        let mut vm = tight_vm();
        vm.load("greedy", "hud.on_draw(function() while true do end end)")
            .expect("load");

        assert_eq!(vm.draw(&state()).len(), 1, "the first frame reports");
        assert!(
            vm.draw(&state()).is_empty(),
            "and the frames it sits out do not"
        );
        assert!(vm.draw(&state()).is_empty());
    }

    #[test]
    fn a_script_that_keeps_going_over_is_switched_off_for_the_session() {
        let mut vm = tight_vm();
        vm.load("greedy", "hud.on_draw(function() while true do end end)")
            .expect("load");

        let mut last = Vec::new();
        // Three strikes, each after a cooldown of three frames.
        for _ in 0..32 {
            let faults = vm.draw(&state());
            if !faults.is_empty() {
                last = faults;
            }
            if vm.is_disabled("greedy") {
                break;
            }
        }
        assert!(vm.is_disabled("greedy"), "it should be off by now");
        assert!(last[0].disabled);
        assert!(
            last[0].message.contains("switched off"),
            "and the player is told it is not coming back: {}",
            last[0].message
        );

        // And it stays quiet rather than reporting for the rest of the session.
        assert!(vm.draw(&state()).is_empty());
    }

    #[test]
    fn one_scripts_fault_leaves_another_scripts_hud_alone() {
        let mut vm = tight_vm();
        vm.load(
            "good",
            "hud.on_draw(function() hud.text{ text = 'still here' } end)",
        )
        .expect("load");
        vm.load("greedy", "hud.on_draw(function() while true do end end)")
            .expect("load");

        let faults = vm.draw(&state());
        assert_eq!(faults.len(), 1);
        assert_eq!(faults[0].mod_id, "greedy");
        let drawn = commands(&vm);
        assert_eq!(drawn.len(), 1, "the good script's work survives");
        let Command::Text { text, .. } = &drawn[0] else {
            panic!("expected text");
        };
        assert_eq!(text, "still here");
    }

    #[test]
    fn a_script_can_hide_the_crosshair_the_engine_draws() {
        let mut vm = vm();
        vm.load(
            "core_ui",
            r#"
hud.on_draw(function()
    hud.hide_builtin("crosshair")
end)
"#,
        )
        .expect("load");
        assert!(vm.draw(&state()).is_empty());
        assert!(
            vm.with_frame(|frame| frame.hides(Builtin::Crosshair))
                .expect("frame")
        );
    }

    #[test]
    fn hiding_chat_is_a_fault_rather_than_a_no_op() {
        // Moderation depends on chat being on the screen. A script asking for
        // it to go is told no, loudly — a silent no-op is how a mod author
        // ships a HUD believing chat is hidden when it is not.
        let mut vm = vm();
        vm.load(
            "hostile",
            "hud.on_draw(function() hud.hide_builtin('chat') end)",
        )
        .expect("load");
        let faults = vm.draw(&state());
        assert_eq!(faults.len(), 1);
        assert!(
            faults[0].message.contains("moderation"),
            "and it says why: {}",
            faults[0].message
        );
    }

    #[test]
    fn a_bar_fed_a_division_by_zero_is_empty_rather_than_infinite() {
        // The reason `Fill` exists. A script writing `health / max_health`
        // against a zero maximum gets a NaN or an infinity, and neither may
        // reach a renderer. `0/0` is not a quantity, so it draws nothing;
        // `1/0` saturates, the same answer `fill = 9999` gives.
        let mut vm = vm();
        vm.load(
            "core_ui",
            r"
hud.on_draw(function()
    hud.bar{ w = 100, h = 8, fill = 0 / 0 }
    hud.bar{ w = 100, h = 8, fill = 1 / 0 }
    hud.bar{ w = 100, h = 8, fill = -1 }
    hud.bar{ w = 100, h = 8, fill = 0.5 }
end)
",
        )
        .expect("load");
        assert!(vm.draw(&state()).is_empty());
        let fills: Vec<Fill> = commands(&vm)
            .iter()
            .map(|command| match command {
                Command::Bar { fill, .. } => *fill,
                other => panic!("expected a bar, got {other:?}"),
            })
            .collect();
        assert_eq!(
            fills,
            vec![Fill::EMPTY, Fill::FULL, Fill::EMPTY, Fill::per_mille(500)]
        );
    }

    #[test]
    fn the_carried_display_is_blocks_and_spare_nodes_the_engine_worked_out() {
        // Charter rule 5, computed once by the engine so every HUD shows the
        // same number. Forty units is 1 block and 13 nodes, not 40 of anything.
        let mut vm = vm();
        vm.load(
            "core_ui",
            r#"
hud.on_draw(function(state)
    local slot = state.carried[1]
    hud.text{ text = slot.name .. " " .. slot.blocks .. "b+" .. slot.nodes .. "n" }
    hud.icon{ material = slot.material, size = 48 }
end)
"#,
        )
        .expect("load");
        assert!(vm.draw(&state()).is_empty());
        let drawn = commands(&vm);
        let Command::Text { text, .. } = &drawn[0] else {
            panic!("expected text");
        };
        assert_eq!(text, "core_blocks:white 1b+13n");
        assert!(matches!(
            drawn[1],
            Command::Icon {
                material: MaterialId(7),
                ..
            }
        ));
    }

    #[test]
    fn a_script_past_the_command_limit_is_told_rather_than_quietly_clipped() {
        let mut vm = HudVm::new(HudLimits {
            frame: crate::hud::Limits {
                commands: 4,
                ..crate::hud::Limits::default()
            },
            ..HudLimits::default()
        })
        .expect("hud vm");
        vm.load(
            "core_ui",
            "hud.on_draw(function() for _ = 1, 10 do hud.text{ text = 'x' } end end)",
        )
        .expect("load");

        let faults = vm.draw(&state());
        assert_eq!(faults.len(), 1);
        assert!(
            faults[0].message.contains("draw commands"),
            "the script is told what it hit: {}",
            faults[0].message
        );
    }

    #[test]
    fn a_server_cannot_push_more_scripts_than_a_client_will_run() {
        let mut vm = HudVm::new(HudLimits {
            scripts: 2,
            ..HudLimits::default()
        })
        .expect("hud vm");
        vm.load("one", "").expect("first");
        vm.load("two", "").expect("second");
        let refused = vm.load("three", "").expect_err("the third is refused");
        assert!(refused.to_string().contains("three"));
    }

    #[test]
    fn a_script_that_registers_nothing_is_not_a_fault() {
        // A mod may push a script that only defines helpers, or one whose HUD
        // is conditional. Drawing nothing is a legitimate thing to do.
        let mut vm = vm();
        vm.load("quiet", "local helper = function() end")
            .expect("load");
        assert!(vm.draw(&state()).is_empty());
        assert!(commands(&vm).is_empty());
    }

    #[test]
    fn a_top_level_that_never_finishes_is_caught_at_load_rather_than_at_the_first_frame() {
        let mut vm = tight_vm();
        let err = vm
            .load("greedy", "while true do end")
            .expect_err("a runaway top level should not hang the client");
        assert!(err.to_string().contains("greedy"));
        assert_eq!(vm.loaded().count(), 0, "and it is not loaded");
    }

    #[test]
    fn an_anchor_a_script_misspelled_is_a_fault_that_names_the_alternatives() {
        let mut vm = vm();
        vm.load(
            "core_ui",
            "hud.on_draw(function() hud.text{ anchor = 'middle' } end)",
        )
        .expect("load");
        let faults = vm.draw(&state());
        assert_eq!(faults.len(), 1);
        assert!(
            faults[0].message.contains("top_left"),
            "a misspelling should be told what the spellings are: {}",
            faults[0].message
        );
    }

    #[test]
    fn both_spellings_of_the_middle_work() {
        let mut vm = vm();
        vm.load(
            "core_ui",
            r"
hud.on_draw(function()
    hud.text{ anchor = 'centre', text = 'a' }
    hud.text{ anchor = 'center', text = 'b' }
end)
",
        )
        .expect("load");
        assert!(vm.draw(&state()).is_empty());
        for command in commands(&vm) {
            let Command::Text { anchor, .. } = command else {
                panic!("expected text");
            };
            assert_eq!(anchor, Anchor::Centre);
        }
    }
}
