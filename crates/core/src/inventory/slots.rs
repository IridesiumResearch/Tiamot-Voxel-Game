// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Slotted inventories, and what a click does to them.
//!
//! # Why slots exist at all
//!
//! A player's carried stacks are CONSOLIDATED — one stack per material, which
//! is the right shape for "what do I have" and the wrong one for a screen. Two
//! half-stacks of the same material cannot exist in a consolidated list, so
//! splitting has nowhere to put the half it made. A view is therefore a fixed
//! run of slots, each holding a stack or nothing.
//!
//! # The server decides, always
//!
//! Every function here runs on the SERVER, against the server's own slots. A
//! client sends [`crate::proto::DialogEvent::Clicked`] — a view, a slot, and
//! which mouse button — and that is a description of a gesture, not an
//! instruction. Nothing here trusts a number that came off the wire beyond
//! using it to look something up, and a lookup that misses is a click on
//! nothing rather than an error.
//!
//! # Charter rule 5 is the whole arithmetic
//!
//! Quantities are UNITS. One block is 27 of them. **Splitting 40 units gives
//! 20 and 20**, not "one and a bit stacks" — the halving is on units and the
//! blocks-and-spares presentation is `display()`'s job and nobody else's. A
//! split that rounded to whole blocks would quietly destroy or invent units,
//! and `proptest` asserts it does not.

use super::Stack;

/// A named run of slots.
///
/// The name is what a mod's `item_slot` and `item_grid` widgets bind to, and is
/// namespaced like every other id (charter rule 8): `player:main` is the
/// engine's, `mymod:chest` is a mod's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    /// Which view this is.
    pub name: String,
    /// Its slots. `None` is an empty slot, which is not the same as a slot
    /// holding zero units — that cannot happen, because an emptied slot is set
    /// to `None`.
    pub slots: Vec<Option<Stack>>,
}

impl View {
    /// An empty view of `count` slots.
    #[must_use]
    pub fn empty(name: impl Into<String>, count: usize) -> Self {
        Self {
            name: name.into(),
            slots: vec![None; count],
        }
    }

    /// The total units this view holds, across every slot.
    ///
    /// `u64` because thirty-six slots of `u32::MAX` overflows a `u32`, and this
    /// is the number conservation is asserted on.
    #[must_use]
    pub fn total_units(&self) -> u64 {
        self.slots
            .iter()
            .flatten()
            .map(|stack| u64::from(stack.units))
            .sum()
    }
}

/// What the player is holding on the cursor, between clicking and clicking again.
///
/// Minecraft's model, and it is the right one: a move is two half-gestures, so
/// the intermediate state has to live somewhere, and a client holding it would
/// be a client that could invent items by lying about what it picked up.
/// **This lives on the server.**
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Grab {
    /// What is on the cursor, if anything.
    pub held: Option<Stack>,
}

/// Every collection of slots a click can reach, and the cursor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Slots {
    /// The views, in a stable order.
    pub views: Vec<View>,
    /// What is on the cursor.
    pub grab: Grab,
}

impl Slots {
    /// Finds a view by name.
    #[must_use]
    pub fn view(&self, name: &str) -> Option<&View> {
        self.views.iter().find(|view| view.name == name)
    }

    /// Finds a view by name, mutably.
    pub fn view_mut(&mut self, name: &str) -> Option<&mut View> {
        self.views.iter_mut().find(|view| view.name == name)
    }

    /// Total units everywhere, cursor included.
    ///
    /// The quantity every operation here must not change. The cursor counts:
    /// a move that dropped what it was carrying would conserve nothing.
    #[must_use]
    pub fn total_units(&self) -> u64 {
        self.views.iter().map(View::total_units).sum::<u64>()
            + self.grab.held.as_ref().map_or(0, |s| u64::from(s.units))
    }

    /// A left click: take the whole stack, or put down what is held.
    ///
    /// Putting down onto a stack of the SAME material merges. Onto a different
    /// one, the two swap — which is what a player expects and what makes a
    /// full inventory rearrangeable without an empty slot to work through.
    ///
    /// Returns whether anything changed, which is what tells the caller to
    /// resend the view.
    pub fn left_click(&mut self, view: &str, index: usize) -> bool {
        let Some(at) = self.locate(view, index) else {
            return false;
        };
        let there = self.views[at].slots[index].take();
        let (slot, hand) = match (self.grab.held.take(), there) {
            // Empty hand on a stack: pick it all up.
            (None, Some(stack)) => (None, Some(stack)),
            // Holding something, empty slot: put it down.
            (Some(held), None) => (Some(held), None),
            // Both full: merge if they are the same material, swap if not.
            (Some(held), Some(there)) => merge_or_swap(held, there),
            (None, None) => return false,
        };
        self.views[at].slots[index] = slot;
        self.grab.held = hand;
        true
    }

    /// A right click: take half, or put down a single unit.
    ///
    /// **The halving is on UNITS.** Charter rule 5: 40 units splits into 20 and
    /// 20, and an odd count leaves the larger half in the hand — 41 gives 21
    /// held and 20 behind — because a player who right-clicks a stack expects
    /// to be holding at least as much as they left.
    pub fn right_click(&mut self, view: &str, index: usize) -> bool {
        let Some(at) = self.locate(view, index) else {
            return false;
        };
        let there = self.views[at].slots[index].take();
        let (slot, hand) = match (self.grab.held.take(), there) {
            (None, Some(mut stack)) => {
                // The half left BEHIND, so the hand keeps the remainder and an
                // odd count rounds the player's way rather than into thin air.
                let behind = stack.units / 2;
                let taken = stack.split(stack.units - behind).ok();
                ((!stack.is_empty()).then_some(stack), taken)
            }
            // One unit down, keeping the rest — the "place one at a time"
            // gesture, on units rather than on blocks.
            (Some(held), there) => place_one(held, there),
            (None, None) => return false,
        };
        self.views[at].slots[index] = slot;
        self.grab.held = hand;
        true
    }

    /// A shift-click: send the stack to the first view that is not this one.
    ///
    /// Merges into matching stacks where they exist, otherwise the first empty
    /// slot. Anything that does not fit stays where it was, which is what makes
    /// shift-clicking into a full container safe rather than lossy.
    pub fn shift_click(&mut self, view: &str, index: usize, into: &str) -> bool {
        if view == into {
            return false;
        }
        let (Some(from), Some(to)) = (self.locate(view, index), self.index_of(into)) else {
            return false;
        };
        if from == to {
            return false;
        }
        let Some(mut moving) = self.views[from].slots[index].take() else {
            return false;
        };

        // Matching stacks first, so shift-clicking twenty units into a view
        // that already holds some tops that up instead of taking a new slot.
        for slot in self.views[to].slots.iter_mut().flatten() {
            if slot.material != moving.material {
                continue;
            }
            let giving = moving.units.min(u32::MAX - slot.units);
            if giving > 0
                && let Ok(part) = moving.split(giving)
            {
                let _ = slot.merge(part);
            }
            if moving.is_empty() {
                break;
            }
        }
        if !moving.is_empty()
            && let Some(empty) = self.views[to].slots.iter_mut().find(|slot| slot.is_none())
        {
            *empty = Some(moving);
            moving.units = 0;
        }

        // Whatever would not fit goes back where it came from, rather than
        // being quietly eaten.
        self.views[from].slots[index] = (!moving.is_empty()).then_some(moving);
        true
    }

    /// The view holding `index`, if both the view and the slot exist.
    ///
    /// **A miss is not an error.** Both numbers came off the wire, and a click
    /// on a slot that is not there is a client describing a screen the server
    /// does not have — which is answered by doing nothing.
    fn locate(&self, view: &str, index: usize) -> Option<usize> {
        let at = self.index_of(view)?;
        (index < self.views[at].slots.len()).then_some(at)
    }

    /// Which view has this name.
    fn index_of(&self, view: &str) -> Option<usize> {
        self.views.iter().position(|held| held.name == view)
    }
}

/// Two stacks meeting in one slot: merge them, or exchange them.
///
/// Returns `(what goes in the slot, what stays in the hand)`.
fn merge_or_swap(held: Stack, there: Stack) -> (Option<Stack>, Option<Stack>) {
    if held.material != there.material {
        // Swap. The hand takes what was there.
        return (Some(held), Some(there));
    }
    let mut merged = there;
    match merged.merge(held) {
        Ok(()) => (Some(merged), None),
        // Overflow: leave both alone rather than lose the difference.
        Err(_) => (Some(merged), Some(held)),
    }
}

/// Putting a single unit down out of a held stack.
///
/// Returns `(what goes in the slot, what stays in the hand)`.
fn place_one(mut held: Stack, there: Option<Stack>) -> (Option<Stack>, Option<Stack>) {
    let Ok(one) = held.split(1) else {
        return ((!held.is_empty()).then_some(held), there);
    };
    let slot = match there {
        None => Some(one),
        Some(mut there) if there.material == one.material => {
            let _ = there.merge(one);
            Some(there)
        }
        // A different material is not somewhere to put one unit; nothing
        // happens and the hand keeps everything it had.
        Some(there) => {
            let _ = held.merge(one);
            Some(there)
        }
    };
    (slot, (!held.is_empty()).then_some(held))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UNITS_PER_BLOCK;
    use crate::material::MaterialId;

    const STONE: MaterialId = MaterialId(2);
    const DIRT: MaterialId = MaterialId(3);

    fn stack(material: MaterialId, units: u32) -> Option<Stack> {
        Stack::new(material, units)
    }

    fn slots(main: Vec<Option<Stack>>) -> Slots {
        Slots {
            views: vec![
                View {
                    name: "player:main".to_owned(),
                    slots: main,
                },
                View::empty("player:hotbar", 9),
            ],
            grab: Grab::default(),
        }
    }

    fn at(slots: &Slots, view: &str, index: usize) -> Option<Stack> {
        slots.view(view).and_then(|v| v.slots[index])
    }

    /// Arbitrary slots, and a run of arbitrary clicks over them.
    ///
    /// Charter rule 15 names conservation as the property to assert with
    /// `proptest`, and this is the one that matters: **no sequence of clicks
    /// may change how many units exist.** Every bug this module could have —
    /// a split that rounds, a swap that drops the hand, a shift-click into a
    /// full view — shows up as a number that moved.
    mod conservation {
        use proptest::prelude::*;

        use super::*;

        fn any_stack() -> impl Strategy<Value = Option<Stack>> {
            prop_oneof![
                1 => Just(None),
                4 => (2u16..6, 1u32..200).prop_map(|(m, units)| Stack::new(MaterialId(m), units)),
            ]
        }

        fn any_slots() -> impl Strategy<Value = Slots> {
            (
                prop::collection::vec(any_stack(), 1..8),
                prop::collection::vec(any_stack(), 1..8),
                any_stack(),
            )
                .prop_map(|(main, hotbar, held)| Slots {
                    views: vec![
                        View {
                            name: "player:main".to_owned(),
                            slots: main,
                        },
                        View {
                            name: "player:hotbar".to_owned(),
                            slots: hotbar,
                        },
                    ],
                    grab: Grab { held },
                })
        }

        /// (which view, which slot, which button)
        fn any_clicks() -> impl Strategy<Value = Vec<(bool, usize, u8)>> {
            prop::collection::vec((any::<bool>(), 0usize..10, 0u8..3), 0..24)
        }

        proptest! {
            #[test]
            fn no_run_of_clicks_changes_how_many_units_exist(
                mut slots in any_slots(),
                clicks in any_clicks(),
            ) {
                let before = slots.total_units();
                for (main, index, button) in clicks {
                    let view = if main { "player:main" } else { "player:hotbar" };
                    let other = if main { "player:hotbar" } else { "player:main" };
                    match button {
                        0 => slots.left_click(view, index),
                        1 => slots.right_click(view, index),
                        _ => slots.shift_click(view, index, other),
                    };
                }
                prop_assert_eq!(
                    slots.total_units(),
                    before,
                    "a run of clicks changed the total"
                );
            }

            /// And no slot ever holds a zero-unit stack, which would render as
            /// an item nobody has.
            #[test]
            fn no_slot_is_ever_left_holding_nothing(
                mut slots in any_slots(),
                clicks in any_clicks(),
            ) {
                for (main, index, button) in clicks {
                    let view = if main { "player:main" } else { "player:hotbar" };
                    let other = if main { "player:hotbar" } else { "player:main" };
                    match button {
                        0 => slots.left_click(view, index),
                        1 => slots.right_click(view, index),
                        _ => slots.shift_click(view, index, other),
                    };
                }
                for view in &slots.views {
                    for slot in view.slots.iter().flatten() {
                        prop_assert!(slot.units > 0, "a slot held a zero stack");
                    }
                }
                if let Some(held) = &slots.grab.held {
                    prop_assert!(held.units > 0, "the cursor held a zero stack");
                }
            }
        }
    }

    #[test]
    fn a_right_click_splits_forty_units_into_twenty_and_twenty() {
        // **Criterion 4, in one assertion.** Charter rule 5: the halving is on
        // UNITS. 40 units is one block and thirteen nodes; splitting it gives
        // 20 and 20 — not "one block each" and not "one block and a bit",
        // either of which would invent or destroy units.
        let mut inv = slots(vec![stack(STONE, 40), None]);
        assert!(inv.right_click("player:main", 0));

        let held = inv.grab.held.expect("something in hand");
        let left = at(&inv, "player:main", 0).expect("something behind");
        assert_eq!(held.units, 20, "the hand should hold half");
        assert_eq!(left.units, 20, "and half should stay");
        assert_eq!(inv.total_units(), 40, "units were invented or destroyed");

        // And the display is blocks plus spares, which is the only place the
        // 27 shows up. 20 units is no whole block and 20 nodes.
        assert_eq!(held.display(), (0, 20));
        // While 40 was one block and thirteen.
        assert_eq!(super::super::display(40), (1, 13));
    }

    #[test]
    fn an_odd_split_leaves_the_larger_half_in_the_hand() {
        // 41 gives 21 held and 20 behind. A player who right-clicks expects to
        // be holding at least as much as they left, and the alternative loses
        // a unit to rounding — which is the bug this rules out.
        let mut inv = slots(vec![stack(STONE, 41), None]);
        inv.right_click("player:main", 0);
        assert_eq!(inv.grab.held.expect("hand").units, 21);
        assert_eq!(at(&inv, "player:main", 0).expect("behind").units, 20);

        // One unit cannot be halved: the hand takes it and the slot empties.
        let mut inv = slots(vec![stack(STONE, 1), None]);
        inv.right_click("player:main", 0);
        assert_eq!(inv.grab.held.expect("hand").units, 1);
        assert!(
            at(&inv, "player:main", 0).is_none(),
            "an emptied slot must be None, never a zero stack"
        );
    }

    #[test]
    fn a_right_click_holding_something_places_one_unit() {
        // The other half of the gesture, and it is ONE UNIT rather than one
        // block: a player placing a single node into a slot is doing sub-block
        // work, which is the whole point of rule 5.
        let mut inv = slots(vec![None, None]);
        inv.grab.held = stack(STONE, UNITS_PER_BLOCK);
        inv.right_click("player:main", 0);
        assert_eq!(at(&inv, "player:main", 0).expect("placed").units, 1);
        assert_eq!(inv.grab.held.expect("hand").units, UNITS_PER_BLOCK - 1);

        // Onto a matching stack it tops it up.
        inv.right_click("player:main", 0);
        assert_eq!(at(&inv, "player:main", 0).expect("placed").units, 2);

        // Onto a DIFFERENT material nothing happens, and nothing is lost.
        let before = inv.total_units();
        let mut inv2 = slots(vec![stack(DIRT, 5), None]);
        inv2.grab.held = stack(STONE, 10);
        inv2.right_click("player:main", 0);
        assert_eq!(at(&inv2, "player:main", 0).expect("still dirt").units, 5);
        assert_eq!(inv2.grab.held.expect("hand").units, 10);
        assert_eq!(inv.total_units(), before);
    }

    #[test]
    fn a_left_click_takes_places_merges_and_swaps() {
        // Take.
        let mut inv = slots(vec![stack(STONE, 30), None]);
        inv.left_click("player:main", 0);
        assert_eq!(inv.grab.held.expect("hand").units, 30);
        assert!(at(&inv, "player:main", 0).is_none());

        // Place into an empty slot.
        inv.left_click("player:main", 1);
        assert!(inv.grab.held.is_none());
        assert_eq!(at(&inv, "player:main", 1).expect("placed").units, 30);

        // Merge onto the same material.
        inv.grab.held = stack(STONE, 5);
        inv.left_click("player:main", 1);
        assert_eq!(at(&inv, "player:main", 1).expect("merged").units, 35);
        assert!(inv.grab.held.is_none());

        // Swap with a different material, which is what makes a full inventory
        // rearrangeable without an empty slot to work through.
        inv.grab.held = stack(DIRT, 7);
        inv.left_click("player:main", 1);
        assert_eq!(at(&inv, "player:main", 1).expect("swapped").material, DIRT);
        assert_eq!(inv.grab.held.expect("hand").material, STONE);
        assert_eq!(inv.grab.held.expect("hand").units, 35);
    }

    #[test]
    fn a_shift_click_fills_matching_stacks_before_taking_a_new_slot() {
        let mut inv = Slots {
            views: vec![
                View {
                    name: "player:main".to_owned(),
                    slots: vec![stack(STONE, 20)],
                },
                View {
                    name: "player:hotbar".to_owned(),
                    slots: vec![stack(STONE, 5), None],
                },
            ],
            grab: Grab::default(),
        };
        assert!(inv.shift_click("player:main", 0, "player:hotbar"));
        assert_eq!(
            at(&inv, "player:hotbar", 0).expect("topped up").units,
            25,
            "it should have merged rather than taken the empty slot"
        );
        assert!(
            at(&inv, "player:hotbar", 1).is_none(),
            "the empty slot was used anyway"
        );
        assert!(
            at(&inv, "player:main", 0).is_none(),
            "the source should be empty"
        );
        assert_eq!(inv.total_units(), 25);
    }

    #[test]
    fn a_shift_click_into_a_full_view_leaves_it_where_it_was() {
        // The lossy failure this rules out: a stack that will not fit must stay
        // put, not vanish.
        let mut inv = Slots {
            views: vec![
                View {
                    name: "player:main".to_owned(),
                    slots: vec![stack(STONE, 20)],
                },
                View {
                    name: "player:hotbar".to_owned(),
                    slots: vec![stack(DIRT, 5)],
                },
            ],
            grab: Grab::default(),
        };
        inv.shift_click("player:main", 0, "player:hotbar");
        assert_eq!(
            at(&inv, "player:main", 0).expect("still there").units,
            20,
            "a stack that did not fit was lost"
        );
        assert_eq!(inv.total_units(), 25);
    }

    #[test]
    fn a_click_on_a_slot_that_is_not_there_does_nothing() {
        // **Both numbers came off the wire.** A client describing a screen the
        // server does not have gets no answer, not an error and not a panic.
        let mut inv = slots(vec![stack(STONE, 10)]);
        let before = inv.clone();
        assert!(
            !inv.left_click("player:main", 99),
            "an absent slot 'changed' something"
        );
        assert!(!inv.right_click("nosuch:view", 0));
        assert!(!inv.left_click("nosuch:view", 0));
        assert!(!inv.shift_click("player:main", 0, "nosuch:view"));
        assert!(
            !inv.shift_click("player:main", 0, "player:main"),
            "into itself"
        );
        assert_eq!(inv.total_units(), before.total_units());
        assert_eq!(
            inv.views, before.views,
            "an impossible click moved something"
        );
    }

    #[test]
    fn an_emptied_slot_is_none_rather_than_a_zero_stack() {
        // A zero-unit stack in a slot would render as an item nobody has and
        // would merge into things as nothing. Every path that empties a slot
        // has to set `None`, and this is the one that checks them together.
        let mut inv = slots(vec![stack(STONE, 4), None]);
        inv.left_click("player:main", 0);
        assert!(at(&inv, "player:main", 0).is_none(), "left click");

        let mut inv = slots(vec![stack(STONE, 1), None]);
        inv.right_click("player:main", 0);
        assert!(at(&inv, "player:main", 0).is_none(), "right click");

        let mut inv = Slots {
            views: vec![
                View {
                    name: "player:main".to_owned(),
                    slots: vec![stack(STONE, 3)],
                },
                View::empty("player:hotbar", 2),
            ],
            grab: Grab::default(),
        };
        inv.shift_click("player:main", 0, "player:hotbar");
        assert!(at(&inv, "player:main", 0).is_none(), "shift click");
    }
}
