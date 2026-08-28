// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! A player's inventory, as it goes to disk.
//!
//! # Why this is not just `postcard::to_allocvec(&Slots)`
//!
//! **Charter rule 8.** A [`Stack`] holds a [`MaterialId`], and that is a
//! RUNTIME id — assigned in this session's registration order and never stable
//! across runs. Writing one to a world file is the fluid-id defect (`7dc37d8`)
//! with a different field: every saved stack would decode as a different
//! material the day a mod's load order changed, silently, because the number is
//! still valid.
//!
//! So the stored form carries **world** ids, translated on the way out and back
//! on the way in, exactly as a chunk's palette is. A stack whose material the
//! world has never heard of cannot be written at all, and is dropped with a
//! warning rather than saved as a number that means something else.
//!
//! # What is stored, and what is not
//!
//! Views by NAME, because a view's position in the list is a fact about the mod
//! set that was loaded — the same reasoning as the ids. A view a player has
//! items in and no mod registers this session is kept in the blob and put back
//! when its mod returns; dropping it would delete somebody's chest for the
//! length of one launch.
//!
//! The cursor stack is stored too. It is where a half-finished move lives, and
//! a player who quit mid-drag should not have paid for it.

use serde::{Deserialize, Serialize};

use crate::inventory::{Grab, Shape, Slots, Stack, View};
use crate::persist::idmap::MaterialMap;

/// The version this build writes.
///
/// **Appending a field is NOT safe** — a blob written before it existed runs
/// out of bytes, which is what `ENTITY_FORMAT_VERSION`'s comment claimed
/// otherwise until it was measured. A new field is a new version and a
/// migration step.
pub const PLAYER_FORMAT_VERSION: u8 = 2;

/// One stack, with a world material id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredStack {
    material: u16,
    units: u32,
    shape: Option<u32>,
    /// A mod's own word for which item this is (format v2).
    ///
    /// **It has to persist or it is not an item's identity.** A sword worn to
    /// half that came back whole after a rejoin would be a durability system
    /// the world forgets, and a named block would be a name that lasts one
    /// session.
    ///
    /// `default` so a v1 row, written before this existed, reads as a plain
    /// stack rather than failing the whole inventory.
    #[serde(default)]
    detail: Option<String>,
}

/// A stack as format v1 wrote one: before a mod could say which item it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredStackV1 {
    material: u16,
    units: u32,
    shape: Option<u32>,
}

/// A view as v1 wrote one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredViewV1 {
    name: String,
    slots: Vec<Option<StoredStackV1>>,
}

/// An inventory as v1 wrote one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct StoredSlotsV1 {
    views: Vec<StoredViewV1>,
    held: Option<StoredStackV1>,
}

impl From<StoredStackV1> for StoredStack {
    fn from(old: StoredStackV1) -> Self {
        Self {
            material: old.material,
            units: old.units,
            shape: old.shape,
            // Nothing said, which is what a plain stack is.
            detail: None,
        }
    }
}

impl From<StoredSlotsV1> for StoredSlots {
    fn from(old: StoredSlotsV1) -> Self {
        Self {
            views: old
                .views
                .into_iter()
                .map(|view| StoredView {
                    name: view.name,
                    slots: view
                        .slots
                        .into_iter()
                        .map(|slot| slot.map(Into::into))
                        .collect(),
                })
                .collect(),
            held: old.held.map(Into::into),
        }
    }
}

/// One view, by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredView {
    name: String,
    slots: Vec<Option<StoredStack>>,
}

/// A player's inventory, as stored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct StoredSlots {
    views: Vec<StoredView>,
    held: Option<StoredStack>,
}

/// Why an inventory could not be written.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlayerDataError {
    /// The blob claims a version this build has no step for.
    #[error("player data is version {version}; this build writes {PLAYER_FORMAT_VERSION}")]
    UnknownVersion {
        /// What the blob claimed.
        version: u8,
    },

    /// The bytes did not decode at the version they claim to be.
    #[error("player data version {version} did not decode")]
    Decode {
        /// What the blob claimed.
        version: u8,
    },
}

/// Turns a session's inventory into bytes, with world ids.
///
/// A stack whose material has no world id is DROPPED rather than written: it
/// cannot be named on disk, and writing its runtime number would be writing a
/// different material. Returns the blob and how many stacks were dropped, so a
/// caller can say so out loud rather than losing them quietly.
#[must_use]
pub fn encode(slots: &Slots, materials: &MaterialMap) -> (Vec<u8>, usize) {
    let mut dropped = 0;
    let mut store = |stack: &Option<Stack>| -> Option<StoredStack> {
        let stack = stack.as_ref()?;
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
    };

    let stored = StoredSlots {
        views: slots
            .views
            .iter()
            .map(|view| StoredView {
                name: view.name.clone(),
                slots: view.slots.iter().map(&mut store).collect(),
            })
            .collect(),
        held: store(&slots.grab.held),
    };
    // Infallible for a structure of owned data; an empty inventory rather than
    // a panic if that ever stops being true.
    (postcard::to_allocvec(&stored).unwrap_or_default(), dropped)
}

/// Turns stored bytes back into a session's inventory.
///
/// `template` is what a fresh player would get — the views this session's mods
/// registered, at the sizes they asked for. Stored views are laid over it by
/// NAME, so a mod that changed a view's size gets its own size and keeps
/// whatever still fits; a stored view no mod registered this session is kept as
/// it was, because deleting somebody's chest because its mod is absent is the
/// thing charter rule 8 exists to prevent.
///
/// # Errors
///
/// [`PlayerDataError`] for a version with no step, or bytes that do not decode.
pub fn decode(
    version: u8,
    bytes: &[u8],
    template: &Slots,
    materials: &MaterialMap,
) -> Result<(Slots, usize), PlayerDataError> {
    // **Old versions are migrated, not refused.** Postcard is not
    // self-describing, so a v1 row read as v2 takes the next stack's first byte
    // as this one's detail tag and every slot after it is nonsense — which is
    // why the version is checked at all. Refusing outright would be worse
    // still: it empties the inventory of everyone who was playing before the
    // upgrade, which is exactly what charter rule 8's round trip is about.
    let stored: StoredSlots = match version {
        PLAYER_FORMAT_VERSION => {
            postcard::from_bytes(bytes).map_err(|_| PlayerDataError::Decode { version })?
        }
        1 => {
            let old: StoredSlotsV1 =
                postcard::from_bytes(bytes).map_err(|_| PlayerDataError::Decode { version })?;
            old.into()
        }
        _ => return Err(PlayerDataError::UnknownVersion { version }),
    };

    let mut dropped = 0;
    let mut load = |stack: &Option<StoredStack>| -> Option<Stack> {
        let stack = stack.as_ref()?;
        let Ok(material) = materials.to_runtime(stack.material) else {
            dropped += 1;
            return None;
        };
        let Some(built) = Stack::new(material, stack.units) else {
            dropped += 1;
            return None;
        };
        Some(Stack {
            shape: stack.shape.and_then(Shape::new),
            detail: stack.detail.clone(),
            ..built
        })
    };

    let mut slots = template.clone();
    // Counted separately and added at the end: `dropped` is borrowed by the
    // closure for as long as it lives, and a slot that did not fit is a
    // different loss from a material that could not be named.
    let mut did_not_fit = 0;
    for stored_view in &stored.views {
        let restored: Vec<Option<Stack>> = stored_view.slots.iter().map(&mut load).collect();
        match slots
            .views
            .iter_mut()
            .find(|view| view.name == stored_view.name)
        {
            Some(view) => {
                // **The session's size wins.** A mod that shrank its container
                // must not get a longer row than it registered, and one that
                // grew it gets the empty slots it asked for.
                did_not_fit += stored_view.slots.len().saturating_sub(view.slots.len());
                for (slot, stack) in view.slots.iter_mut().zip(restored) {
                    *slot = stack;
                }
            }
            None => slots.views.push(View {
                name: stored_view.name.clone(),
                slots: restored,
            }),
        }
    }
    slots.grab = Grab {
        held: load(&stored.held),
    };
    // `load` borrows `dropped`, so the two counts are added after it goes out
    // of scope at the end of this expression.
    let lost = dropped + did_not_fit;
    Ok((slots, lost))
}

/// The material a stack is made of, for a test to name.
#[cfg(test)]
const fn material_of(stack: &Stack) -> crate::material::MaterialId {
    stack.material
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::material::MaterialId;

    /// A map where runtime and world ids differ, which is the whole point.
    fn shifted() -> MaterialMap {
        // Runtime 1 is world 7, runtime 2 is world 4: deliberately not the
        // identity, so a test cannot pass by ignoring the translation.
        MaterialMap::from_pairs(&[(MaterialId(1), 7), (MaterialId(2), 4)])
    }

    fn template() -> Slots {
        Slots {
            views: vec![View {
                name: "player:main".to_owned(),
                slots: vec![None; 4],
            }],
            grab: Grab::default(),
        }
    }

    #[test]
    fn an_inventory_round_trips_through_world_ids() {
        let mut slots = template();
        slots.views[0].slots[0] = Stack::new(MaterialId(1), 27);
        slots.views[0].slots[2] = Stack::new(MaterialId(2), 13);
        slots.grab.held = Stack::new(MaterialId(1), 5);

        let (bytes, dropped) = encode(&slots, &shifted());
        assert_eq!(dropped, 0);
        let (back, lost) = decode(PLAYER_FORMAT_VERSION, &bytes, &template(), &shifted())
            .expect("it should decode");

        assert_eq!(lost, 0);
        assert_eq!(back, slots, "what came back is not what went in");
    }

    #[test]
    fn the_stored_ids_are_the_worlds_and_not_this_sessions() {
        // **The defect this file exists to prevent**, asserted on the bytes.
        // A runtime id written to disk decodes as a different material the day
        // a mod's load order changes — silently, because the number is still
        // valid. `7dc37d8` is what that cost for fluid.
        let mut slots = template();
        slots.views[0].slots[0] = Stack::new(MaterialId(1), 27);
        let (bytes, _) = encode(&slots, &shifted());

        // World 7, not runtime 1.
        assert!(
            bytes.contains(&7),
            "the world id is not in the blob: {bytes:?}"
        );

        // And read back under a DIFFERENT session numbering, it is still the
        // same material: world 7 is runtime 3 here.
        let renumbered = MaterialMap::from_pairs(&[(MaterialId(3), 7)]);
        let (back, _) = decode(PLAYER_FORMAT_VERSION, &bytes, &template(), &renumbered)
            .expect("it should decode");
        assert_eq!(
            back.views[0].slots[0].as_ref().map(material_of),
            Some(MaterialId(3)),
            "the stack came back as this session's id for the same world material"
        );
    }

    #[test]
    fn a_detail_survives_the_trip() {
        // **An item's identity has to persist or it is not one.** A sword worn
        // to half that came back whole after a rejoin is a durability system
        // the world forgets.
        let mut slots = template();
        let mut stack = Stack::new(MaterialId(2), 1).expect("stack");
        stack.detail = Some("worn=7".to_owned());
        assert!(slots.insert("player:main", stack));

        let (bytes, _) = encode(&slots, &shifted());
        let (back, lost) =
            decode(PLAYER_FORMAT_VERSION, &bytes, &template(), &shifted()).expect("decode");
        assert_eq!(lost, 0);
        assert_eq!(
            back.view("player:main")
                .and_then(|view| view.slots[0].as_ref())
                .and_then(|stack| stack.detail.clone()),
            Some("worn=7".to_owned()),
            "a mod's own word for which item this is did not survive the save"
        );
    }

    #[test]
    fn an_inventory_written_before_details_existed_still_loads() {
        // **Migrated, not refused.** Postcard is not self-describing, so a v1
        // row read as v2 takes the next stack's first byte as this one's
        // detail tag — which is why the version is checked. Refusing outright
        // would empty the inventory of everyone who was playing before the
        // upgrade, which is the opposite of charter rule 8's round trip.
        let old = StoredSlotsV1 {
            views: vec![StoredViewV1 {
                name: "player:main".to_owned(),
                slots: vec![
                    Some(StoredStackV1 {
                        // A WORLD id, which `shifted` maps back to runtime 1.
                        material: 7,
                        units: 30,
                        shape: Some(0b101),
                    }),
                    None,
                ],
            }],
            held: None,
        };
        let bytes = postcard::to_allocvec(&old).expect("encode v1");

        let (back, lost) = decode(1, &bytes, &template(), &shifted()).expect("decode v1");
        assert_eq!(lost, 0, "a v1 inventory lost stacks on the way in");
        let stack = back
            .view("player:main")
            .and_then(|view| view.slots[0].as_ref())
            .expect("the v1 stack");
        assert_eq!(stack.units, 30);
        assert_eq!(
            stack.shape.map(Shape::occupancy),
            Some(0b101),
            "the cut did not survive the migration"
        );
        assert_eq!(
            stack.detail, None,
            "a v1 stack said nothing, so it says nothing"
        );
    }

    #[test]
    fn a_shape_survives_the_trip() {
        // A cut is what only crafting produces, so losing it on a restart would
        // destroy work that cannot be dug back up.
        let mut slots = template();
        let shape = Shape::new(0b101).expect("a mask of two cells");
        slots.views[0].slots[1] = Stack::new(MaterialId(1), 2).map(|stack| Stack {
            shape: Some(shape),
            ..stack
        });

        let (bytes, _) = encode(&slots, &shifted());
        let (back, _) = decode(PLAYER_FORMAT_VERSION, &bytes, &template(), &shifted())
            .expect("it should decode");
        assert_eq!(
            back.views[0].slots[1]
                .as_ref()
                .and_then(|stack| stack.shape),
            Some(shape)
        );
    }

    #[test]
    fn a_material_the_world_cannot_name_is_dropped_rather_than_renumbered() {
        // Runtime 9 has no world id. Writing its number would write a stack of
        // whatever world material 9 happens to be.
        let mut slots = template();
        slots.views[0].slots[0] = Stack::new(MaterialId(9), 27);
        let (bytes, dropped) = encode(&slots, &shifted());
        assert_eq!(dropped, 1, "the caller has to be able to say so out loud");

        let (back, _) = decode(PLAYER_FORMAT_VERSION, &bytes, &template(), &shifted())
            .expect("it should decode");
        assert_eq!(back.views[0].slots[0], None);
    }

    #[test]
    fn a_view_no_mod_registers_this_session_is_kept_rather_than_deleted() {
        // Charter rule 8's rule for materials, applied to containers: somebody
        // who turns a mod off for one launch should not come back to an empty
        // chest.
        let mut slots = template();
        slots.views.push(View {
            name: "somemod:chest".to_owned(),
            slots: vec![Stack::new(MaterialId(1), 27), None],
        });

        let (bytes, _) = encode(&slots, &shifted());
        let (back, _) = decode(PLAYER_FORMAT_VERSION, &bytes, &template(), &shifted())
            .expect("it should decode");

        let chest = back
            .views
            .iter()
            .find(|view| view.name == "somemod:chest")
            .expect("the absent mod's view survived");
        assert_eq!(
            chest.slots[0].as_ref().map(material_of),
            Some(MaterialId(1))
        );
    }

    #[test]
    fn a_view_that_shrank_keeps_what_still_fits() {
        let mut wide = template();
        wide.views[0].slots = vec![None; 8];
        wide.views[0].slots[7] = Stack::new(MaterialId(1), 27);
        wide.views[0].slots[1] = Stack::new(MaterialId(2), 9);
        let (bytes, _) = encode(&wide, &shifted());

        // This session registers four slots, not eight.
        let (back, lost) = decode(PLAYER_FORMAT_VERSION, &bytes, &template(), &shifted())
            .expect("it should decode");
        assert_eq!(back.views[0].slots.len(), 4, "the session's size wins");
        assert_eq!(
            back.views[0].slots[1].as_ref().map(material_of),
            Some(MaterialId(2))
        );
        assert!(lost > 0, "the caller has to hear that a slot did not fit");
    }

    #[test]
    fn a_version_with_no_step_is_an_error_rather_than_a_guess() {
        let (bytes, _) = encode(&template(), &shifted());
        assert_eq!(
            decode(PLAYER_FORMAT_VERSION + 1, &bytes, &template(), &shifted()),
            Err(PlayerDataError::UnknownVersion {
                version: PLAYER_FORMAT_VERSION + 1
            })
        );
    }

    #[test]
    fn rubbish_is_an_error_rather_than_a_panic() {
        assert!(matches!(
            decode(PLAYER_FORMAT_VERSION, &[0xff; 16], &template(), &shifted()),
            Err(PlayerDataError::Decode { .. })
        ));
    }
}
