// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Fluid: what is in a block, and which fluid it is.
//!
//! # The model, and what it deliberately is not
//!
//! Classic Minecraft-style flow: source blocks that sustain themselves, a level
//! that decays with distance, and no conservation. Milk poured from a source is
//! created out of nothing and drained milk goes nowhere. That is a settled scope
//! decision, not an oversight — a conserved, pressure-equalising sim is a
//! different and much larger system, and the API here is shaped so one could
//! replace the update rule later without touching storage, the wire, or the
//! renderer.
//!
//! # Block resolution, on purpose
//!
//! Everything else in this engine is sub-node. Fluid is not, and that keeps it
//! off the sub-node risk surface entirely: there are no partially-flooded
//! carved blocks, no fluid-versus-occupancy interactions in the mesher, and no
//! new cases in collision. Sub-Node Contract §4 states the whole of it in one
//! sentence — a block accepts fluid **iff its occupancy is empty**.
//!
//! # Why a byte and not a nibble
//!
//! The level needs three bits and the source flag one. The remaining four hold
//! the fluid's id, because the engine supports several registered fluids even
//! though the reference mods ship one; without an id a chunk could not say
//! whether a block held milk or something a mod added next to it. A byte per
//! block is 4 KiB for a chunk that has any fluid at all and **nothing at all for
//! a chunk that has none**, which is almost every chunk — see [`FluidLayer`].

use crate::material::MaterialId;

mod layer;
mod solver;

pub mod codec;

pub use layer::FluidLayer;
pub use solver::{Blocked, Flow, Neighbourhood, Solver, Tuning};

/// The fullest a fluid block can be.
///
/// Levels run `1..=7`, with 0 meaning "no fluid". Seven is a source or a block
/// directly fed by one; each block of lateral travel costs one.
pub const MAX_LEVEL: u8 = 7;

/// The most fluids that can be registered at once.
///
/// Four bits of id, and zero means "none", so fifteen. Not a limit anybody is
/// expected to reach — the reference mods register one — and raising it means
/// widening the per-block byte, which is a storage and protocol change rather
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

/// What one block holds: which fluid, how much, and whether it is a source.
///
/// # Layout
///
/// | bits | meaning |
/// |---|---|
/// | 4..8 | fluid id, 0 for none |
/// | 3 | source flag |
/// | 0..3 | level, 0..=7 |
///
/// A source always reads at [`MAX_LEVEL`]; the flag says it sustains itself
/// rather than draining. The two are stored separately rather than encoding a
/// source as "level 8" so that a decayed source and a full flow block stay
/// distinguishable, which is what makes draining terminate.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Fluid(pub u8);

impl Fluid {
    /// A block with nothing in it.
    pub const EMPTY: Self = Self(0);

    /// A block of `fluid` at `level`, not a source.
    ///
    /// A level of zero, or a fluid of [`FluidId::NONE`], is [`Self::EMPTY`]:
    /// "some milk, but none of it" is not a state worth being able to write.
    #[must_use]
    pub const fn flowing(fluid: FluidId, level: u8) -> Self {
        if fluid.is_none() || level == 0 {
            return Self::EMPTY;
        }
        let level = if level > MAX_LEVEL { MAX_LEVEL } else { level };
        Self((fluid.0 << 4) | level)
    }

    /// A source block of `fluid`, which sustains [`MAX_LEVEL`].
    #[must_use]
    pub const fn source(fluid: FluidId) -> Self {
        if fluid.is_none() {
            return Self::EMPTY;
        }
        Self((fluid.0 << 4) | 0b1000 | MAX_LEVEL)
    }

    /// Which fluid this is, or [`FluidId::NONE`].
    #[must_use]
    pub const fn fluid(self) -> FluidId {
        FluidId(self.0 >> 4)
    }

    /// How full the block is, `0..=7`.
    #[must_use]
    pub const fn level(self) -> u8 {
        self.0 & 0b0111
    }

    /// Whether this block sustains itself.
    #[must_use]
    pub const fn is_source(self) -> bool {
        self.0 & 0b1000 != 0
    }

    /// Whether the block holds nothing.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.level() == 0 || self.fluid().is_none()
    }

    /// How much of the block's HEIGHT this fills, in twenty-sevenths.
    ///
    /// **In sub-node units even though fluid is block-resolution**, because the
    /// only things that ask are the renderer's surface height and the physics'
    /// submerged fraction, and both of those already speak in cells (charter
    /// rule 5). A full block is 24 of 27 rather than 27: a brim-full block of
    /// milk still has a visible surface below the block above it, which is what
    /// makes a waterfall read as milk rather than as a solid column.
    #[must_use]
    pub const fn depth_units(self) -> u32 {
        if self.is_empty() {
            return 0;
        }
        // Level 7 is 24/27, about 0.9 of a block; level 1 is 3/27, a ninth.
        // A fraction rather than a cell count: a block is only three sub-nodes
        // tall, and the whole point of this number is to express surface
        // heights the lattice cannot.
        (self.level() as u32) * 24 / (MAX_LEVEL as u32)
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
    /// How far a source spreads sideways on flat ground, in blocks.
    ///
    /// Seven for milk, which is [`MAX_LEVEL`]: each block of travel costs a
    /// level, so a shorter range is a fluid that thins out faster.
    pub flow_range: u8,
    /// How full a block must be before this fluid treats it as floor, in cells
    /// of 27. See [`Tuning::waterlogs_at`].
    pub waterlogs_at: u32,
    /// Simulation ticks between updates of this fluid.
    ///
    /// One means every fluid tick. Larger is a slower, more viscous fluid, and
    /// it costs proportionally less to simulate.
    pub tick_rate: u8,
    /// How many neighbouring sources make a block a source of its own.
    ///
    /// Zero never renews. See [`Tuning::renews_from`] — it is what stops an
    /// ocean collapsing when somebody takes a bucket out of the middle of it.
    pub renews_from: u8,
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
            // Spreads nowhere and moves never: the fluid's own rules left with
            // the mod that knew them, and guessing at them would rearrange a
            // world the moment somebody disabled something.
            flow_range: 0,
            waterlogs_at: crate::UNITS_PER_BLOCK,
            tick_rate: 1,
            renews_from: 0,
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
    fn a_source_and_a_full_flow_block_are_not_the_same_thing() {
        // **The distinction draining depends on.** A source sustains itself; a
        // flow block at the same level drains the moment its parent goes away.
        // Encoding a source as "level 8" would make these one value and there
        // would be nothing left to drain.
        let milk = FluidId(1);
        let source = Fluid::source(milk);
        let full = Fluid::flowing(milk, MAX_LEVEL);

        assert_eq!(source.level(), full.level());
        assert!(source.is_source());
        assert!(!full.is_source());
        assert_ne!(source, full);
    }

    #[test]
    fn a_level_of_nothing_is_empty_whichever_way_it_is_written() {
        let milk = FluidId(1);
        assert_eq!(Fluid::flowing(milk, 0), Fluid::EMPTY);
        assert_eq!(Fluid::flowing(FluidId::NONE, MAX_LEVEL), Fluid::EMPTY);
        assert!(Fluid::EMPTY.is_empty());
        assert_eq!(Fluid::EMPTY.depth_units(), 0);
    }

    #[test]
    fn a_full_block_stops_short_of_the_block_above() {
        // Contract §4 and the reason a waterfall reads as milk: a brim-full
        // block still shows a surface.
        let milk = FluidId(1);
        assert_eq!(Fluid::flowing(milk, MAX_LEVEL).depth_units(), 24);
        assert!(Fluid::flowing(milk, MAX_LEVEL).depth_units() < crate::UNITS_PER_BLOCK);
        // Monotone, and never zero for a block that holds anything.
        for level in 1..=MAX_LEVEL {
            let shallower = Fluid::flowing(milk, level - 1).depth_units();
            let here = Fluid::flowing(milk, level).depth_units();
            assert!(here > 0);
            assert!(
                here > shallower,
                "level {level} is no deeper than {}",
                level - 1
            );
        }
    }

    #[test]
    fn ids_are_assigned_in_registration_order_and_zero_is_never_one() {
        let mut fluids = Fluids::new();
        let milk = fluids
            .register(Registered {
                name: "core:milk".into(),
                flow_range: 7,
                waterlogs_at: 14,
                renews_from: 0,
                color: [255, 255, 255],
                tick_rate: 1,
                material: MaterialId(4),
            })
            .expect("first registration");
        assert_eq!(milk, FluidId(1), "zero is reserved for 'no fluid'");
        assert_eq!(fluids.id_of("core:milk"), Some(milk));
        assert_eq!(fluids.get(milk).map(|f| f.flow_range), Some(7));
        assert!(fluids.get(FluidId::NONE).is_none());
    }

    #[test]
    fn the_same_name_cannot_be_registered_twice() {
        let mut fluids = Fluids::new();
        let entry = || Registered {
            name: "core:milk".into(),
            flow_range: 7,
            waterlogs_at: 14,
            renews_from: 0,
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
                    flow_range: 7,
                    waterlogs_at: 14,
                    renews_from: 0,
                    color: [255, 255, 255],
                    tick_rate: 1,
                    material: MaterialId(1),
                })
                .expect("within the limit");
        }
        assert!(matches!(
            fluids.register(Registered {
                name: "test:one-too-many".into(),
                flow_range: 7,
                waterlogs_at: 14,
                renews_from: 0,
                color: [255, 255, 255],
                tick_rate: 1,
                material: MaterialId(1),
            }),
            Err(RegisterError::Full { .. })
        ));
    }

    #[test]
    fn a_block_round_trips_through_its_byte() {
        for id in 1..=MAX_FLUIDS as u8 {
            for level in 1..=MAX_LEVEL {
                let flowing = Fluid::flowing(FluidId(id), level);
                assert_eq!(flowing.fluid(), FluidId(id));
                assert_eq!(flowing.level(), level);
                assert!(!flowing.is_source());
            }
            let source = Fluid::source(FluidId(id));
            assert_eq!(source.fluid(), FluidId(id));
            assert_eq!(source.level(), MAX_LEVEL);
            assert!(source.is_source());
        }
    }
}
