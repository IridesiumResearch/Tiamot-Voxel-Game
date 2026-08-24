// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! What a HUD script is allowed to know.

use crate::material::MaterialId;

/// One entry of what the player is carrying.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Carried {
    /// What it is, for [`super::Command::Icon`].
    pub material: MaterialId,
    /// Its string id, for a script that wants to show a name (charter rule 8:
    /// the string is canonical, the number is per-session).
    pub name: String,
    /// How much, in units.
    pub units: u32,
    /// The 27-bit occupancy each item is cut to, or `0` for loose material.
    ///
    /// A HUD showing a hotbar wants to draw a stair differently from a block of
    /// the same stone, and this is the only thing that tells them apart.
    pub shape: u32,
}

impl Carried {
    /// Whole blocks and spare nodes, charter rule 5's display.
    ///
    /// Computed by the engine and handed over ready, rather than left to every
    /// script to divide by 27 itself. That is what makes "respects the 27-unit
    /// display everywhere" a property of the engine and not a convention mods
    /// are asked to follow.
    #[must_use]
    pub const fn display(&self) -> (u32, u32) {
        crate::inventory::display(self.units)
    }
}

/// What the crosshair is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Look {
    /// The sub-node cell being looked at.
    pub cell: [i32; 3],
    /// What that cell is made of.
    pub material: MaterialId,
    /// Its string id.
    pub name: String,
}

/// The tool in hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldTool {
    /// Qualified id.
    pub id: String,
    /// What a player should be shown.
    pub name: String,
    /// Whether a click takes a cell or a whole block.
    pub brush: String,
}

/// Everything a HUD script may read, rebuilt each frame.
///
/// # Read-only, and small on purpose
///
/// A HUD draws what the player can already see. It does not need the terrain,
/// the other players, or anything it could use to answer a question the game
/// has not answered for the player — a script that could read block data the
/// client has streamed but not drawn is an x-ray cheat with a mod's blessing.
///
/// So this is the player's own situation and nothing else, and it grows only
/// when a HUD genuinely cannot be drawn without something.
///
/// # Floats here are fine
///
/// Charter rule 4 scopes determinism to simulation. This is presentation, it is
/// one-way — no HUD value can reach simulation state — and a position rendered
/// as text is exactly the case the rule's scope paragraph exempts.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct State {
    /// Where the player is, in world blocks.
    pub position: [f64; 3],
    /// Where they are looking, in degrees.
    pub yaw: f32,
    /// Where they are looking, in degrees.
    pub pitch: f32,
    /// Time of day, from 0 at midnight through 0.5 at noon.
    pub time_of_day: f32,
    /// Which entry of [`State::carried`] is selected, zero-based.
    pub selected: usize,
    /// What the player is carrying, in slot order.
    pub carried: Vec<Carried>,
    /// What the crosshair is on, if anything is in reach.
    pub looking_at: Option<Look>,
    /// How far along a dig is, if one is happening.
    pub dig: Option<super::Fill>,
    /// The tool in hand, if any is registered.
    pub tool: Option<HeldTool>,
}
