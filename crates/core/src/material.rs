// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Material identity: the string ⇄ numeric id mapping.
//!
//! Charter rule 8 governs this module. String ids like `"core:white"` are
//! canonical and stable forever; numeric [`MaterialId`]s are a per-session
//! interning of them and are **never** stable across runs. The world database
//! owns the authoritative table and remaps on load.
//!
//! The consequence worth internalising: a `MaterialId` is only meaningful
//! alongside the [`Registry`] that issued it. Never persist one, never send one
//! to another process, and never hard-code one — except [`MaterialId::AIR`],
//! which is reserved.
//!
//! Full registry semantics — manifest scan, dependency resolution, the
//! registration window, and the FREEZE that makes `register` a hard error
//! (charter rule 9) — arrive in Task 05. What is here is deliberately the
//! minimum that the voxel data model needs, behind [`MaterialRegistry`] so the
//! real implementation can replace it without touching callers.

use std::collections::BTreeMap;

/// Runtime material id, interned by a [`Registry`] for one session.
///
/// Zero is reserved for air, so [`MaterialId::default`] is air and a
/// zeroed buffer decodes as empty space rather than as an arbitrary material.
///
/// Ordering is by numeric id. That is an arbitrary but *stable within a
/// session* order, which is what deterministic output ordering needs — see
/// [`crate::inventory::break_block`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct MaterialId(pub u16);

impl MaterialId {
    /// Air. Reserved as id 0 and never issued by [`Registry::register`].
    ///
    /// Air is a real material rather than an absence: a block of air is
    /// `Uniform(AIR)`, and sub-node occupancy is defined as "not air". Treating
    /// it as a material is what keeps the 27-unit arithmetic free of special
    /// cases (charter rule 5).
    pub const AIR: Self = Self(0);

    /// The placeholder every unregistered string id maps to, reserved as id 1.
    ///
    /// Charter rule 8: content authored against a mod that is not currently
    /// loaded must round-trip byte-for-byte rather than being destroyed, so
    /// removing a mod and re-adding it is not a data-loss event. The world
    /// database preserves the original string alongside this id.
    pub const UNKNOWN: Self = Self(1);

    /// Whether this is air.
    #[must_use]
    pub const fn is_air(self) -> bool {
        self.0 == Self::AIR.0
    }

    /// The raw numeric value.
    ///
    /// Only for storage layers that must serialise the session table alongside
    /// it. Anything else wanting to name a material should hold the string id.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

/// Canonical string id for [`MaterialId::AIR`].
pub const AIR_NAME: &str = "engine:air";

/// Canonical string id for [`MaterialId::UNKNOWN`].
pub const UNKNOWN_NAME: &str = "engine:unknown";

/// Why a registration failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    /// The numeric id space is exhausted.
    #[error("material id space exhausted at {max} entries")]
    Exhausted {
        /// How many materials can exist in one session.
        max: usize,
    },

    /// A reserved string id was offered for registration.
    #[error("`{name}` is reserved by the engine and cannot be registered")]
    Reserved {
        /// The offered name.
        name: String,
    },
}

/// Read-only view of a material table.
///
/// Deliberately minimal. Task 05 replaces [`Registry`] with the real
/// mod-aware implementation; everything that only needs to resolve names takes
/// this trait and will not need changing.
pub trait MaterialRegistry {
    /// Numeric id for a string id, or `None` if it was never registered.
    fn id_of(&self, name: &str) -> Option<MaterialId>;

    /// String id for a numeric id, or `None` if this registry never issued it.
    fn name_of(&self, id: MaterialId) -> Option<&str>;

    /// How many materials are registered, including the two reserved ones.
    fn len(&self) -> usize;

    /// Whether the registry holds nothing but the reserved materials.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Resolve a string id, falling back to [`MaterialId::UNKNOWN`].
    ///
    /// This is the charter rule 8 read path: an id belonging to an absent mod
    /// resolves to the placeholder rather than failing, so a world referencing
    /// it still loads.
    fn id_or_unknown(&self, name: &str) -> MaterialId {
        self.id_of(name).unwrap_or(MaterialId::UNKNOWN)
    }
}

/// In-memory material table.
///
/// Ids are issued sequentially from 2; 0 and 1 are reserved for
/// [`MaterialId::AIR`] and [`MaterialId::UNKNOWN`]. Registering the same name
/// twice is idempotent and returns the existing id.
///
/// A [`BTreeMap`] rather than a `HashMap`, deliberately: Rust's default hasher
/// is randomly seeded per process, so `HashMap` iteration order is not stable
/// even between two runs on one machine. Charter rule 4 bans that from anything
/// a simulation result depends on, and a registry is exactly that.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    /// Indexed by numeric id.
    names: Vec<String>,
    ids: BTreeMap<String, MaterialId>,
}

impl Registry {
    /// Maximum materials in one session, bounded by [`MaterialId`]'s width.
    pub const MAX_MATERIALS: usize = u16::MAX as usize + 1;

    /// A registry holding only the reserved materials.
    #[must_use]
    pub fn new() -> Self {
        let mut registry = Self {
            names: Vec::new(),
            ids: BTreeMap::new(),
        };
        // Order matters: these must land on ids 0 and 1.
        registry.names.push(AIR_NAME.to_owned());
        registry.ids.insert(AIR_NAME.to_owned(), MaterialId::AIR);
        registry.names.push(UNKNOWN_NAME.to_owned());
        registry
            .ids
            .insert(UNKNOWN_NAME.to_owned(), MaterialId::UNKNOWN);
        registry
    }

    /// Interns a string id, returning its numeric id.
    ///
    /// Idempotent: registering a name already present returns the id it already
    /// has, rather than issuing a second one.
    ///
    /// # Errors
    ///
    /// [`RegistryError::Reserved`] if `name` is one of the engine's reserved
    /// ids, and [`RegistryError::Exhausted`] if the id space is full.
    pub fn register(&mut self, name: &str) -> Result<MaterialId, RegistryError> {
        if let Some(&existing) = self.ids.get(name) {
            if existing == MaterialId::AIR || existing == MaterialId::UNKNOWN {
                return Err(RegistryError::Reserved {
                    name: name.to_owned(),
                });
            }
            return Ok(existing);
        }

        if self.names.len() >= Self::MAX_MATERIALS {
            return Err(RegistryError::Exhausted {
                max: Self::MAX_MATERIALS,
            });
        }

        let id = MaterialId(self.names.len() as u16);
        self.names.push(name.to_owned());
        self.ids.insert(name.to_owned(), id);
        Ok(id)
    }

    /// Iterates `(id, name)` in ascending id order.
    pub fn iter(&self) -> impl Iterator<Item = (MaterialId, &str)> {
        self.names
            .iter()
            .enumerate()
            .map(|(index, name)| (MaterialId(index as u16), name.as_str()))
    }
}

impl MaterialRegistry for Registry {
    fn id_of(&self, name: &str) -> Option<MaterialId> {
        self.ids.get(name).copied()
    }

    fn name_of(&self, id: MaterialId) -> Option<&str> {
        self.names.get(id.0 as usize).map(String::as_str)
    }

    fn len(&self) -> usize {
        self.names.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn air_is_zero_and_default() {
        assert_eq!(MaterialId::AIR.get(), 0);
        assert_eq!(MaterialId::default(), MaterialId::AIR);
        assert!(MaterialId::AIR.is_air());
        assert!(!MaterialId::UNKNOWN.is_air());
    }

    #[test]
    fn reserved_ids_are_present_in_a_fresh_registry() {
        let registry = Registry::new();
        assert_eq!(registry.id_of(AIR_NAME), Some(MaterialId::AIR));
        assert_eq!(registry.id_of(UNKNOWN_NAME), Some(MaterialId::UNKNOWN));
        assert_eq!(registry.name_of(MaterialId::AIR), Some(AIR_NAME));
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn registration_issues_ids_from_two_upwards() {
        let mut registry = Registry::new();
        assert_eq!(registry.register("core:white"), Ok(MaterialId(2)));
        assert_eq!(registry.register("core:stone"), Ok(MaterialId(3)));
    }

    #[test]
    fn registration_is_idempotent() {
        let mut registry = Registry::new();
        let first = registry.register("core:white").expect("register");
        let second = registry.register("core:white").expect("re-register");
        assert_eq!(first, second);
        assert_eq!(registry.len(), 3, "no second entry should be created");
    }

    #[test]
    fn reserved_names_cannot_be_registered() {
        let mut registry = Registry::new();
        assert!(matches!(
            registry.register(AIR_NAME),
            Err(RegistryError::Reserved { .. })
        ));
        assert!(matches!(
            registry.register(UNKNOWN_NAME),
            Err(RegistryError::Reserved { .. })
        ));
    }

    #[test]
    fn unregistered_names_resolve_to_the_unknown_placeholder() {
        // Charter rule 8: content from an absent mod must survive a load rather
        // than being silently destroyed.
        let registry = Registry::new();
        assert_eq!(registry.id_of("somemod:absent"), None);
        assert_eq!(
            registry.id_or_unknown("somemod:absent"),
            MaterialId::UNKNOWN
        );
    }

    #[test]
    fn iteration_is_in_ascending_id_order() {
        let mut registry = Registry::new();
        // Register in an order that is not alphabetical, to prove iteration
        // follows id rather than name.
        registry.register("core:zinc").expect("register");
        registry.register("core:alpha").expect("register");

        let ids: Vec<_> = registry.iter().map(|(id, _)| id.get()).collect();
        assert_eq!(ids, vec![0, 1, 2, 3]);
        let names: Vec<_> = registry.iter().map(|(_, name)| name).collect();
        assert_eq!(
            names,
            vec![AIR_NAME, UNKNOWN_NAME, "core:zinc", "core:alpha"]
        );
    }
}
