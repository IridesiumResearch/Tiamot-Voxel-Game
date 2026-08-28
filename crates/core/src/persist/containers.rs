// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Inventories that belong to the world rather than to a player.
//!
//! A chest, a furnace, a hopper. The engine owns the slots — stacking,
//! conservation and the material id map are all its rules — and a mod owns what
//! the container MEANS: where it is, what may go in it, who may open it.
//!
//! # Why this is not `game.storage`
//!
//! A mod could serialise its own chests into its own key-value store. It would
//! then be reimplementing stacking, unit conservation and the string-to-numeric
//! id map (charter rule 8), and getting one of the three wrong somewhere the
//! engine would have got it right. What a mod cannot express is exactly what
//! this is for.
//!
//! # The id map, again
//!
//! Stored in WORLD ids like everything else that reaches disk, and translated
//! on the way in and out. A runtime id in a save is the fluid defect (`7dc37d8`)
//! waiting to happen: still a valid number, and the wrong material, the day a
//! mod's load order changes.

use serde::{Deserialize, Serialize};

use crate::inventory::{Shape, Stack, View};
use crate::material::MaterialId;
use crate::persist::idmap::MaterialMap;

/// The version this build writes.
///
/// Bumped for any change to what is stored. An older version is migrated
/// rather than refused — see [`decode`].
pub const CONTAINER_FORMAT_VERSION: u8 = 1;

/// One stack, in world ids.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredStack {
    material: u16,
    units: u32,
    shape: Option<u32>,
    detail: Option<String>,
}

/// One container's contents.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct StoredContainer {
    slots: Vec<Option<StoredStack>>,
}

/// Why a container could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ContainerError {
    /// The blob claims a version this build has no step for.
    #[error("container data is version {version}; this build writes {CONTAINER_FORMAT_VERSION}")]
    UnknownVersion {
        /// What the row said.
        version: u8,
    },

    /// The bytes did not decode as that version.
    #[error("container data version {version} did not decode")]
    Decode {
        /// What the row said.
        version: u8,
    },
}

/// Encodes a container, and reports how many stacks it could not name.
///
/// A stack whose material this world has no id for is dropped rather than
/// failing the whole container: the alternative is a chest that cannot be saved
/// at all because one thing in it came from a mod somebody removed.
#[must_use]
pub fn encode(view: &View, materials: &MaterialMap) -> (Vec<u8>, usize) {
    let mut dropped = 0;
    let stored = StoredContainer {
        slots: view
            .slots
            .iter()
            .map(|slot| {
                let stack = slot.as_ref()?;
                let Ok(material) = materials.to_world(stack.material) else {
                    dropped += 1;
                    return None;
                };
                Some(StoredStack {
                    material,
                    units: stack.units,
                    shape: stack.shape.map(Shape::occupancy),
                    detail: stack.detail.clone(),
                })
            })
            .collect(),
    };
    // Never fails for a type this crate defines; an empty container on a write
    // error would be worse than one that is not written at all.
    (postcard::to_allocvec(&stored).unwrap_or_default(), dropped)
}

/// Decodes a container into a view of `slots` places, in runtime ids.
///
/// The size is the mod's, not the blob's: a mod that made its chests bigger
/// should find its old ones grown rather than refused, and one that made them
/// smaller gets what still fits — with the rest reported as dropped rather
/// than silently gone.
///
/// # Errors
///
/// [`ContainerError`] for a version with no step, or bytes that do not decode.
pub fn decode(
    version: u8,
    bytes: &[u8],
    name: &str,
    slots: usize,
    materials: &MaterialMap,
) -> Result<(View, usize), ContainerError> {
    if version != CONTAINER_FORMAT_VERSION {
        return Err(ContainerError::UnknownVersion { version });
    }
    let stored: StoredContainer =
        postcard::from_bytes(bytes).map_err(|_| ContainerError::Decode { version })?;

    let mut dropped = 0;
    let mut view = View::empty(name, slots);
    for (index, slot) in stored.slots.iter().enumerate() {
        let Some(stack) = slot else { continue };
        let Ok(material) = materials.to_runtime(stack.material) else {
            dropped += 1;
            continue;
        };
        let Some(built) = Stack::new(material, stack.units) else {
            dropped += 1;
            continue;
        };
        let built = Stack {
            shape: stack.shape.and_then(Shape::new),
            detail: stack.detail.clone(),
            ..built
        };
        match view.slots.get_mut(index) {
            Some(place) => *place = Some(built),
            // The container shrank. Counted rather than dropped in silence, so
            // an operator can be told that a mod's change cost somebody a row
            // of their chest.
            None => dropped += 1,
        }
    }
    Ok((view, dropped))
}

/// The empty container a name refers to before anything is put in it.
#[must_use]
pub fn empty(name: &str, slots: usize) -> View {
    View::empty(name, slots)
}

/// Whether a material id is one this world can name, for a caller checking
/// before it writes.
#[must_use]
pub fn nameable(material: MaterialId, materials: &MaterialMap) -> bool {
    materials.to_world(material).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shifted() -> MaterialMap {
        // Deliberately not the identity, so a test cannot pass by ignoring the
        // translation — the same fixture the player codec uses.
        MaterialMap::from_pairs(&[(MaterialId(1), 7), (MaterialId(2), 4)])
    }

    #[test]
    fn a_container_survives_the_trip_with_its_cuts_and_details() {
        let mut view = View::empty("core_chest:at:1,2,3", 4);
        view.slots[0] = Stack::new(MaterialId(1), 30);
        view.slots[2] = Stack::new(MaterialId(2), 5).map(|stack| Stack {
            shape: Shape::new(0b101),
            detail: Some("wear=3".to_owned()),
            ..stack
        });

        let (bytes, dropped) = encode(&view, &shifted());
        assert_eq!(dropped, 0);
        let (back, lost) = decode(
            CONTAINER_FORMAT_VERSION,
            &bytes,
            "core_chest:at:1,2,3",
            4,
            &shifted(),
        )
        .expect("decode");
        assert_eq!(lost, 0);
        assert_eq!(back, view, "a container came back as something else");
    }

    #[test]
    fn a_material_this_world_cannot_name_costs_one_stack_and_not_the_chest() {
        // A mod removed since the chest was filled. Refusing the whole
        // container would lose everything else in it, which is the opposite of
        // charter rule 8's round trip.
        let mut view = View::empty("core_chest:at:1,2,3", 3);
        view.slots[0] = Stack::new(MaterialId(1), 10);
        view.slots[1] = Stack::new(MaterialId(99), 10);

        let (bytes, dropped) = encode(&view, &shifted());
        assert_eq!(dropped, 1, "the unnameable stack should be the only loss");
        let (back, _) = decode(
            CONTAINER_FORMAT_VERSION,
            &bytes,
            "core_chest:at:1,2,3",
            3,
            &shifted(),
        )
        .expect("decode");
        assert_eq!(
            back.slots[0].as_ref().map(|stack| stack.units),
            Some(10),
            "the stack beside it was lost too"
        );
    }

    #[test]
    fn a_container_that_shrank_reports_what_did_not_fit() {
        let mut view = View::empty("core_chest:at:1,2,3", 4);
        view.slots[3] = Stack::new(MaterialId(1), 10);
        let (bytes, _) = encode(&view, &shifted());

        let (back, lost) = decode(
            CONTAINER_FORMAT_VERSION,
            &bytes,
            "core_chest:at:1,2,3",
            2,
            &shifted(),
        )
        .expect("decode");
        assert_eq!(back.slots.len(), 2, "the mod's size wins, not the blob's");
        assert_eq!(lost, 1, "a row that no longer fits must be reported");
    }

    #[test]
    fn a_version_with_no_step_is_refused_rather_than_guessed_at() {
        let (bytes, _) = encode(&View::empty("x", 1), &shifted());
        assert_eq!(
            decode(99, &bytes, "x", 1, &shifted()),
            Err(ContainerError::UnknownVersion { version: 99 })
        );
    }
}
