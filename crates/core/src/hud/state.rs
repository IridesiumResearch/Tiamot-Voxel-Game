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
    /// A mod's own word for which item this is, if it said one.
    ///
    /// A durability bar under a slot, or a name over it. Opaque to the engine
    /// — see [`crate::inventory::Stack::detail`].
    pub detail: Option<String>,
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

    /// How many items this is, or `None` for loose material.
    ///
    /// See [`crate::inventory::items`]: a cut is counted, loose rubble is
    /// measured, and which of the two a script should show is not a decision
    /// every script should be making separately.
    #[must_use]
    pub const fn count(&self) -> Option<u32> {
        crate::inventory::items(self.units, self.shape)
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
    /// The hotbar: the player's own slots, in slot order, holes included.
    ///
    /// **A place, not a list.** These are the first slots of `player:main`,
    /// which is what the number keys select between — so slot four is slot four
    /// whether or not there is anything in it, and picking something up does
    /// not shuffle what the other keys reach. It used to be the CONSOLIDATED
    /// inventory, one entry per material, and a player who dug a second thing
    /// watched their hotbar rearrange itself under their hands.
    pub carried: Vec<Option<Carried>>,
    /// What is in the off-hand, if anything.
    ///
    /// A slot of `player:main` like any other — the twenty-eighth — reached
    /// with a key rather than by dragging, and handed to a script separately
    /// because a HUD draws it somewhere else entirely.
    pub offhand: Option<Carried>,
    /// What the crosshair is on, if anything is in reach.
    pub looking_at: Option<Look>,
    /// How far along a dig is, if one is happening.
    pub dig: Option<super::Fill>,
    /// The tool in hand, if any is registered.
    pub tool: Option<HeldTool>,
    /// What each mod has sent to its OWN HUD script, by mod id.
    ///
    /// **The engine has no health bar, no hunger bar and no experience bar,
    /// and should not.** Charter rule 1 puts what those mean in a mod — and a
    /// mod that could compute them and not draw them would be a mod that could
    /// not finish the job. This is the channel: the server-side half sets
    /// values per player, and a script sees the ones its own mod sent under
    /// `state.values`.
    ///
    /// Keyed by mod id here and flattened by the VM, so a script cannot read
    /// another mod's values — the same isolation `game.storage` has, and for
    /// the same reason: it is a property of the surface rather than of good
    /// behaviour.
    pub values: std::collections::BTreeMap<String, Values>,
}

/// One mod's HUD values, by name.
pub type Values = std::collections::BTreeMap<String, Value>;

/// A value a mod sends to its own HUD script.
///
/// # Why these three and nothing nested
///
/// A HUD draws numbers, words and switches. A nested structure would be a
/// second serialisation format on a path where **the client decodes what a
/// server it does not trust sent it** (charter rule 14), and every one of its
/// depths would be a bound to check. A mod with something structured to say
/// flattens it into keys, which is what it would have to do to draw it anyway.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Value {
    /// A quantity: hit points, hunger, a countdown, a fraction.
    Number(f64),
    /// A word: a status, a name, a formatted total.
    Text(String),
    /// A switch: poisoned, sneaking, in combat.
    Flag(bool),
}

/// The seam a mod sets its own HUD values through.
///
/// One method, and the mod id is passed by the VM rather than by the script —
/// the same isolation [`crate::storage::Access`] has, and for the same reason:
/// a mod cannot name another's values because there is nowhere in the surface
/// to put the name.
pub trait Access: Send + Sync {
    /// Replaces what one mod wants one player's HUD to show.
    ///
    /// Returns whether the player was there to tell. Replacing rather than
    /// merging, because a mod computes what it wants shown and says so —
    /// merging makes "this value is gone now" impossible to express.
    fn set_hud(&self, mod_id: &str, player: [u8; 32], values: Values) -> bool;
}

/// How many values one mod may send one player.
///
/// Checked where they are SET, so a mod hears about it, and again where they
/// are decoded, because the second is reading what a server sent.
pub const MAX_VALUES: usize = 32;

/// The longest a value's name may be.
pub const MAX_KEY: usize = 32;

/// The longest a [`Value::Text`] may be.
///
/// A HUD line, not a paragraph: anything longer is a mod trying to send a
/// document through a channel sized for a label.
pub const MAX_TEXT: usize = 64;
