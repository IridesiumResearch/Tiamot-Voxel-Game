// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Fluid: what is in a block, and which fluid it is.
//!
//! # The model
//!
//! **Conserved.** A block holds a volume in cells of 27 — the unit charter rule
//! 5 uses for everything else — and volume moves between blocks rather than
//! being created. It leaves the world only through a declared sink, and the
//! solver counts every cell it destroys so charter rule 15's conservation
//! proptest can be written at all.
//!
//! There are no source blocks. An infinite spring is a conservation violation
//! by definition, so the source flag, `flow_range` and `renews_from` all went
//! with the model that needed them. Sub-Node Contract §4 is authoritative and
//! §4.1 records what this replaced and why.
//!
//! # Block resolution, on purpose
//!
//! Everything else in this engine is sub-node. Fluid is not: one volume per
//! block, never a per-cell mask. The lattice is read for exactly two things —
//! how much will fit (`capacity`) and whether a block is floor — and that keeps
//! fluid off the sub-node risk surface entirely.
//!
//! **Volume in cells is not sub-node fluid.** The unit changed; the resolution
//! did not. What it buys is that a block one third full of stone holds one
//! third less fluid, which the old sevenths could not express and which
//! conservation makes observable: a bucket is a measurement, so a player pouring
//! into chiselled ground could otherwise get more back out than they put in.
//!
//! # Why two bytes and not one
//!
//! Volume needs five bits to reach 27 and the fluid id needs four. Nine bits do
//! not fit in a byte, and the two ways to make them fit are both worse than
//! paying for the second one: halving the fluid registry to seven trades a
//! limit that lasts forever against a byte, and dropping to 15 volumes
//! reintroduces the conversion this change exists to delete.
//!
//! Two bytes per block is 8 KiB for a chunk that has any fluid at all and
//! **nothing at all for a chunk that has none**, which is almost every chunk —
//! see [`FluidLayer`].

use crate::material::MaterialId;

mod layer;
mod solver;

pub mod codec;

pub use layer::FluidLayer;
pub use solver::{Blocked, Flow, Neighbourhood, Solver, Tuning};

/// The fullest a block with nothing else in it can be, in cells of 27.
///
/// A block's actual ceiling is [`capacity`], which subtracts whatever terrain is
/// in the way. This is the ceiling for an empty one.
pub const MAX_VOLUME: u32 = 27;

/// How much fluid a block will take, in cells of 27.
///
/// **Sub-Node Contract §4.** `occupancy` is how full of terrain the block is, in
/// the same cells; `waterlogs_at` is the registering fluid's threshold for
/// calling a block floor. At or above that threshold the block is fluid-solid —
/// it neither holds nor passes fluid — and below it, what is left over is what
/// will fit.
///
/// The world reports a fact and the fluid decides what it means, which is why
/// the threshold is passed in rather than known here: two fluids in one world
/// may disagree about what counts as floor.
#[must_use]
pub const fn capacity(occupancy: u32, waterlogs_at: u32) -> u32 {
    if occupancy >= waterlogs_at {
        return 0;
    }
    if occupancy >= MAX_VOLUME {
        return 0;
    }
    MAX_VOLUME - occupancy
}

/// The most fluids that can be registered at once.
///
/// Four bits of id, and zero means "none", so fifteen. Not a limit anybody is
/// expected to reach — the reference mods register one — and raising it means
/// widening the per-block word, which is a storage and protocol change rather
/// than a constant.
pub const MAX_FLUIDS: usize = 15;

/// Which registered fluid a block holds.
///
/// Zero is "none". Ids are assigned at registration and are **per session**,
/// exactly like material ids (charter rule 8): the string id is canonical and
/// the world database owns the mapping.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FluidId(pub u8);

impl FluidId {
    /// No fluid.
    pub const NONE: Self = Self(0);

    /// Whether this names a fluid at all.
    #[must_use]
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }
}

/// What one block holds: which fluid, and how much.
///
/// # Layout
///
/// | bits | meaning |
/// |---|---|
/// | 5..9 | fluid id, 0 for none |
/// | 0..5 | volume in cells, `0..=27` |
///
/// The seven high bits are spare. There is no source flag: under a conserved
/// model a block that sustains itself is matter from nothing.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Fluid(pub u16);

/// Where the fluid id sits.
const ID_SHIFT: u16 = 5;

/// The volume field, once the id is shifted off it.
const VOLUME_MASK: u16 = 0b1_1111;

impl Fluid {
    /// A block with nothing in it.
    pub const EMPTY: Self = Self(0);

    /// A block of `fluid` holding `volume` cells.
    ///
    /// A volume of zero, or a fluid of [`FluidId::NONE`], is [`Self::EMPTY`]:
    /// "some milk, but none of it" is not a state worth being able to write.
    /// Volume is clamped to [`MAX_VOLUME`] rather than wrapping, because a
    /// value that does not fit the field would otherwise silently become a
    /// different fluid.
    #[must_use]
    pub const fn new(fluid: FluidId, volume: u32) -> Self {
        if fluid.is_none() || volume == 0 {
            return Self::EMPTY;
        }
        let volume = if volume > MAX_VOLUME {
            MAX_VOLUME
        } else {
            volume
        };
        Self(((fluid.0 as u16) << ID_SHIFT) | volume as u16)
    }

    /// Which fluid this is, or [`FluidId::NONE`].
    #[must_use]
    pub const fn fluid(self) -> FluidId {
        FluidId((self.0 >> ID_SHIFT) as u8)
    }

    /// How much it holds, in cells of 27.
    ///
    /// **This is the number the mesher's surface height and the physics'
    /// submerged fraction both want**, in the units they already speak
    /// (charter rule 5). It used to need converting from sevenths; Sub-Node
    /// Contract §4.1 records the conversion that deleted.
    #[must_use]
    pub const fn volume(self) -> u32 {
        (self.0 & VOLUME_MASK) as u32
    }

    /// Whether the block holds nothing.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.volume() == 0 || self.fluid().is_none()
    }

    /// The same fluid holding `volume` instead.
    ///
    /// Empties the block rather than keeping an id with no volume behind it.
    #[must_use]
    pub const fn with_volume(self, volume: u32) -> Self {
        Self::new(self.fluid(), volume)
    }
}

/// Somewhere a mod can read and write fluid.
///
/// # Why a trait, and why it writes
///
/// The same seam [`crate::light::LightSource`] is: the store lives in the
/// server and the script VM lives in core, which cannot depend on it (charter
/// rule 3). This one also *writes*, because `game.set_fluid` is how a mod pours
/// anything at all — a read-only view would leave the reference mod unable to
/// do the one thing it exists to demonstrate.
///
/// # Interior mutability, and why it is not a hazard here
///
/// `&self` rather than `&mut self`, because the VM hands the same handle to
/// every mod environment and cannot lend it out mutably. Every caller is the
/// simulation thread inside a tick — a mod callback runs there and nowhere else
/// — so the lock behind this is uncontended and is never held across a
/// callback, which is the arrangement that would deadlock.
pub trait Access: Send + Sync {
    /// What a block holds, or [`Fluid::EMPTY`] where nothing is loaded.
    ///
    /// Empty rather than an error for somewhere nobody is, exactly as light
    /// answers dark: a mod asking about unloaded terrain gets the honest answer
    /// that there is no milk there, and an `Option` would push that judgement
    /// onto every caller.
    fn fluid_at(&self, pos: crate::BlockPos) -> Fluid;

    /// Records what a block holds and wakes the flow around it.
    ///
    /// Returns whether anything changed. A write to a block that cannot accept
    /// fluid is not refused here — the next fluid tick clears it, which is the
    /// same answer the solver gives when somebody builds in a pond, and having
    /// one rule rather than two is worth more than the early error.
    fn set_fluid_at(&self, pos: crate::BlockPos, value: Fluid) -> bool;

    /// The id registered under a string name, or `None`.
    ///
    /// String ids are canonical and numeric ones are per session (charter rule
    /// 8), so a mod names its fluid and the engine resolves it — never the
    /// other way round.
    fn fluid_id(&self, name: &str) -> Option<FluidId>;
}

/// Everything registered about one fluid.
///
/// The engine holds only what it must simulate and draw with. Anything that is
/// a game decision — how fast it hurts, what it sounds like landing on stone —
/// belongs to the mod that registered it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registered {
    /// The canonical string id, `"core:milk"`.
    pub name: String,
    /// How full a block must be before this fluid treats it as floor, in cells
    /// of 27. See [`Tuning::waterlogs_at`].
    pub waterlogs_at: u32,
    /// Simulation ticks between updates of this fluid.
    ///
    /// One means every fluid tick. Larger is a slower, more viscous fluid, and
    /// it costs proportionally less to simulate.
    pub tick_rate: u8,
    /// One in how many fluid ticks an exposed block loses a cell, or zero.
    ///
    /// **A declared sink** (Sub-Node Contract §4.3). Only a block with air
    /// directly above it evaporates, so a wide shallow pool goes before a deep
    /// narrow one — more of it is exposed. Zero never evaporates, which is the
    /// engine default: a world that only ever gets wetter is a mod's decision
    /// to make, not the engine's.
    pub evaporates: u32,
    /// What being inside it looks like, sRGB `0..=255`.
    ///
    /// Separate from the material, because a texture is what the surface looks
    /// like from outside and this is what the world looks like from within.
    pub color: [u8; 3],
    /// The material a full block of it is drawn as.
    ///
    /// Fluid has no material of its own in the block store — a block holds
    /// terrain and fluid independently — so this is what the mesher and the
    /// texture atlas look up.
    pub material: MaterialId,
}

/// Every fluid the mods registered, by id.
///
/// Frozen with the rest of the registries (charter rule 9). A flat `Vec` rather
/// than a map for the same reason [`crate::light::Emissions`] is one: ids are
/// dense, assigned in registration order, and the solver reads this per block
/// visited.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Fluids {
    by_id: Vec<Registered>,
    /// Ids that stand in for a fluid the world knows and no mod registered.
    ///
    /// See [`Fluids::register_placeholder`]. A `BTreeSet` rather than a flag on
    /// [`Registered`] so the wire type stays exactly what a mod declared —
    /// a placeholder is a fact about *this session*, not about the fluid.
    placeholders: std::collections::BTreeSet<FluidId>,
}

impl Fluids {
    /// An empty registry — a world whose mods registered no fluid at all.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            by_id: Vec::new(),
            placeholders: std::collections::BTreeSet::new(),
        }
    }

    /// Registers a fluid and returns its id.
    ///
    /// # Errors
    ///
    /// [`RegisterError`] if the name is already taken or the table is full.
    pub fn register(&mut self, fluid: Registered) -> Result<FluidId, RegisterError> {
        if self.by_id.iter().any(|known| known.name == fluid.name) {
            return Err(RegisterError::Duplicate { name: fluid.name });
        }
        if self.by_id.len() >= MAX_FLUIDS {
            return Err(RegisterError::Full { name: fluid.name });
        }
        self.by_id.push(fluid);
        // Ids start at one, because zero means "no fluid".
        Ok(FluidId(self.by_id.len() as u8))
    }

    /// Registers a stand-in for a fluid the world knows and no mod supplied.
    ///
    /// # Why an absent mod's fluid still gets an id
    ///
    /// Charter rule 8: unregistered ids map to a preserved placeholder and data
    /// round-trips byte-for-byte. Materials do this by registering behaviourless
    /// aliases (see [`crate::persist::idmap::IdTable::reconcile`]), and fluid
    /// needs it more, not less — a stored fluid byte has only four bits of id,
    /// so a world id with no session id could not even be held in memory, and
    /// the only alternative to a stand-in is discarding somebody's lake because
    /// they disabled a mod.
    ///
    /// The result is inert by construction: it spreads nowhere, is drawn as air,
    /// and [`Fluids::is_placeholder`] lets the simulation leave it alone. It
    /// exists to occupy an id so the bytes survive, and for no other reason —
    /// put the mod back and the same blocks are milk again.
    ///
    /// # Errors
    ///
    /// [`RegisterError`] as [`Fluids::register`]. **Placeholders share the
    /// fifteen ids with real fluids**, so a world that has accumulated fifteen
    /// fluids across its history leaves none for a new mod. That is a real
    /// limit and a loud error is the only honest response to it.
    pub fn register_placeholder(&mut self, name: &str) -> Result<FluidId, RegisterError> {
        let id = self.register(Registered {
            name: name.to_owned(),
            // Treats everything as floor and never evaporates: the fluid's own
            // rules left with the mod that knew them, and guessing at them
            // would rearrange a world the moment somebody disabled something.
            waterlogs_at: 1,
            tick_rate: 1,
            evaporates: 0,
            // Never drawn and never submerged in, so the colour is arbitrary;
            // white is the one that cannot be mistaken for a deliberate choice.
            color: [255, 255, 255],
            // Air, so a client that is somehow told about it draws nothing.
            material: MaterialId::AIR,
        })?;
        self.placeholders.insert(id);
        Ok(id)
    }

    /// Whether this id is standing in for a fluid no mod registered.
    #[must_use]
    pub fn is_placeholder(&self, id: FluidId) -> bool {
        self.placeholders.contains(&id)
    }

    /// Every fluid a mod actually registered this session, with its id.
    ///
    /// What the simulation and the wire table want. [`Fluids::iter`] yields
    /// placeholders too, because the persistence layer has to see them.
    pub fn iter_registered(&self) -> impl Iterator<Item = (FluidId, &Registered)> {
        self.iter().filter(|(id, _)| !self.is_placeholder(*id))
    }

    /// What was registered under an id, if anything.
    #[must_use]
    pub fn get(&self, id: FluidId) -> Option<&Registered> {
        if id.is_none() {
            return None;
        }
        self.by_id.get(id.0 as usize - 1)
    }

    /// The id registered under a string name.
    #[must_use]
    pub fn id_of(&self, name: &str) -> Option<FluidId> {
        self.by_id
            .iter()
            .position(|known| known.name == name)
            .map(|index| FluidId(index as u8 + 1))
    }

    /// How many fluids are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Whether no fluid was registered at all.
    ///
    /// The whole fluid system can be skipped for such a world, which is the
    /// common case for a mod set that only defines terrain.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Every registered fluid with its id, in registration order.
    pub fn iter(&self) -> impl Iterator<Item = (FluidId, &Registered)> {
        self.by_id
            .iter()
            .enumerate()
            .map(|(index, fluid)| (FluidId(index as u8 + 1), fluid))
    }
}

/// Why a fluid registration was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegisterError {
    /// Two mods claimed the same string id.
    #[error("a fluid named {name} is already registered")]
    Duplicate {
        /// The name that was claimed twice.
        name: String,
    },

    /// More than [`MAX_FLUIDS`] fluids.
    #[error("cannot register {name}: only {MAX_FLUIDS} fluids fit in a block's four id bits")]
    Full {
        /// The name that did not fit.
        name: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_volume_of_nothing_is_empty_whichever_way_it_is_written() {
        let milk = FluidId(1);
        assert_eq!(Fluid::new(milk, 0), Fluid::EMPTY);
        assert_eq!(Fluid::new(FluidId::NONE, MAX_VOLUME), Fluid::EMPTY);
        assert!(Fluid::EMPTY.is_empty());
        assert_eq!(Fluid::EMPTY.volume(), 0);
    }

    #[test]
    fn volume_and_id_survive_each_other_across_the_whole_range() {
        // **The bit-packing test that matters.** Volume needs five bits and the
        // id four; getting the shift wrong makes a deep block of one fluid read
        // as a shallow block of another, which looks like a flow bug rather
        // than a storage one.
        for id in 1..=MAX_FLUIDS as u8 {
            for volume in 1..=MAX_VOLUME {
                let value = Fluid::new(FluidId(id), volume);
                assert_eq!(value.fluid(), FluidId(id), "id lost at volume {volume}");
                assert_eq!(value.volume(), volume, "volume lost for id {id}");
                assert!(!value.is_empty());
            }
        }
    }

    #[test]
    fn more_than_a_block_holds_is_clamped_rather_than_wrapped() {
        // Wrapping would carry into the id field, so a block overfilled by one
        // cell would silently become a different fluid.
        let milk = FluidId(1);
        let over = Fluid::new(milk, MAX_VOLUME + 5);
        assert_eq!(over.volume(), MAX_VOLUME);
        assert_eq!(over.fluid(), milk);
    }

    #[test]
    fn terrain_in_a_block_takes_the_space_out_of_its_capacity() {
        // **Sub-Node Contract §4.1, the volume lie retired.** A block a third
        // full of stone holds a third less, and conservation is what made that
        // observable: a bucket is a measurement.
        let waterlogs_at = 14;
        assert_eq!(capacity(0, waterlogs_at), MAX_VOLUME);
        assert_eq!(capacity(9, waterlogs_at), MAX_VOLUME - 9);
        assert_eq!(capacity(13, waterlogs_at), MAX_VOLUME - 13);
        // At the threshold the block is floor, and floor holds nothing.
        assert_eq!(capacity(waterlogs_at, waterlogs_at), 0);
        assert_eq!(capacity(MAX_VOLUME, waterlogs_at), 0);
    }

    #[test]
    fn a_fluid_that_calls_everything_floor_still_fills_empty_air() {
        // `waterlogs_at = 1` is the old "any chiselled cell is waterproof"
        // rule, and it must still let fluid into a block with nothing in it or
        // the fluid could never move at all.
        assert_eq!(capacity(0, 1), MAX_VOLUME);
        assert_eq!(capacity(1, 1), 0);
    }

    #[test]
    fn ids_are_assigned_in_registration_order_and_zero_is_never_one() {
        let mut fluids = Fluids::new();
        let milk = fluids
            .register(Registered {
                name: "core:milk".into(),
                waterlogs_at: 14,
                evaporates: 0,
                color: [255, 255, 255],
                tick_rate: 1,
                material: MaterialId(4),
            })
            .expect("first registration");
        assert_eq!(milk, FluidId(1), "zero is reserved for 'no fluid'");
        assert_eq!(fluids.id_of("core:milk"), Some(milk));
        assert_eq!(fluids.get(milk).map(|f| f.waterlogs_at), Some(14));
        assert!(fluids.get(FluidId::NONE).is_none());
    }

    #[test]
    fn the_same_name_cannot_be_registered_twice() {
        let mut fluids = Fluids::new();
        let entry = || Registered {
            name: "core:milk".into(),
            waterlogs_at: 14,
            evaporates: 0,
            color: [255, 255, 255],
            tick_rate: 1,
            material: MaterialId(4),
        };
        fluids.register(entry()).expect("first");
        assert!(matches!(
            fluids.register(entry()),
            Err(RegisterError::Duplicate { .. })
        ));
    }

    #[test]
    fn the_id_field_is_the_registry_limit() {
        let mut fluids = Fluids::new();
        for index in 0..MAX_FLUIDS {
            fluids
                .register(Registered {
                    name: format!("test:fluid{index}"),
                    waterlogs_at: 14,
                    evaporates: 0,
                    color: [255, 255, 255],
                    tick_rate: 1,
                    material: MaterialId(1),
                })
                .expect("within the limit");
        }
        assert!(matches!(
            fluids.register(Registered {
                name: "test:one-too-many".into(),
                waterlogs_at: 14,
                evaporates: 0,
                color: [255, 255, 255],
                tick_rate: 1,
                material: MaterialId(1),
            }),
            Err(RegisterError::Full { .. })
        ));
    }
}
