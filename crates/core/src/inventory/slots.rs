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
use crate::material::MaterialId;

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

/// A view a mod asked the engine to give every player.
///
/// See [`Slots::for_player_with`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewDef {
    /// The qualified id, e.g. `"core_armour:worn"`.
    pub id: String,
    /// How many slots it has. Fixed: an armour rack does not grow.
    pub slots: usize,
}

/// The most slots one registered view may have.
///
/// **A bound rather than a taste.** Every slot of every view is sent to the
/// client on any change, so a mod asking for a million would be a mod that
/// stops the server for everybody. Large enough for any rack, chest page or
/// bandolier somebody actually wants.
pub const MAX_VIEW_SLOTS: usize = 256;

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

/// The engine's own view names.
///
/// Namespaced `player:` because charter rule 8 applies to these as much as to a
/// mod's, and because a mod naming its container `main` must not collide.
pub const PLAYER_MAIN: &str = "player:main";
/// How many of `player:main`'s slots the number keys reach.
///
/// **The hotbar is a place in the player's own inventory, not a second view.**
/// It used to be `player:hotbar`, nine slots beside the twenty-seven — so a
/// player had to shuffle things between two grids to put them where a key
/// could reach them, and the HUD, which drew what was CARRIED rather than what
/// was in those slots, agreed with neither. One view, and the first nine slots
/// of it are the ones the keys select between.
pub const PLAYER_HOTBAR_SLOTS: usize = 9;

/// How many slots `player:main` starts with.
///
/// **It used to start at zero**, on the reasoning that [`Slots::insert`] grows
/// it and so nothing could ever be capped. That was true and still is — but
/// nothing DISPLAYED it then. A dialog showing a grid over a view with two
/// slots in it draws twenty-five boxes a player cannot put anything into,
/// because a click on a slot that is not there is correctly ignored. Reported
/// from the window as "items do not seem to display in my inventory yet".
///
/// So a player starts with room, and `insert` still grows past it. This caps
/// nothing; it only means the room exists before something is in it.
pub const PLAYER_MAIN_SLOTS: usize = 28;

/// Which of `player:main`'s slots is the off-hand.
///
/// **A slot, not a second view.** The hotbar is already a band of this one view
/// (see [`PLAYER_HOTBAR_SLOTS`]) and the off-hand is one more place in it — so
/// `game.inventory` sees it like anything else, conservation counts it like
/// anything else, and there is no second grid for a player to shuffle things
/// between. It sits past the twenty-seven a screen shows, which is why it is
/// reached with a key rather than by dragging.
pub const PLAYER_OFFHAND_SLOT: usize = 27;

impl Slots {
    /// The views every player has.
    ///
    /// **One**, and its first [`PLAYER_HOTBAR_SLOTS`] are the hotbar.
    ///
    /// `player:main` starts at [`PLAYER_MAIN_SLOTS`] and GROWS beyond it as
    /// material arrives — see [`Slots::insert`].
    #[must_use]
    pub fn for_player() -> Self {
        Self::for_player_with(&[])
    }

    /// The same, plus the views mods asked for.
    ///
    /// # Why a mod cannot just start using a name
    ///
    /// **A view is a place, and a place has a size.** `player:main` grows on
    /// insert because a player may own more than fits (see [`Slots::insert`]),
    /// but an armour rack does not: four slots is the point of it, and a fifth
    /// appearing because something was shoved in would be a rack that is no
    /// longer four. So the engine has to be told how many there are, and the
    /// only moment that can be settled for every player at once is the
    /// registration window (charter rule 9).
    ///
    /// What the slots MEAN — that this one is a helmet and that one is not — is
    /// entirely the mod's. The engine moves stacks between slots and has no
    /// opinion about which ones are boots.
    #[must_use]
    pub fn for_player_with(extra: &[ViewDef]) -> Self {
        let mut views = vec![View::empty(PLAYER_MAIN, PLAYER_MAIN_SLOTS)];
        for def in extra {
            // A mod that names `player:main` gets ignored rather than obeyed:
            // resizing the view the engine itself credits digging into is not
            // something a mod may do by picking a string.
            if def.id == PLAYER_MAIN {
                continue;
            }
            views.push(View::empty(def.id.clone(), def.slots));
        }
        Self {
            views,
            grab: Grab::default(),
        }
    }

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

    /// Puts whatever is on the cursor back into the inventory.
    ///
    /// **A screen closing must not leave a stack in hand.** The cursor is where
    /// a half-finished move lives, and a player who picks something up and
    /// presses Escape has not agreed to put it anywhere — so it goes back
    /// wherever it fits rather than staying in a place with no picture.
    ///
    /// Returns whether anything moved. What will not fit stays on the cursor,
    /// which is better than destroying it: an inventory grows, so this only
    /// fails for a view that has been removed under the player.
    pub fn return_held(&mut self, view: &str) -> bool {
        let Some(held) = self.grab.held.take() else {
            return false;
        };
        if self.insert(view, held.clone()) {
            return true;
        }
        // Nowhere to put it. Back on the cursor rather than gone — the next
        // screen the player opens can still see it.
        self.grab.held = Some(held);
        false
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
            // Shape as well as material — see `Slots::insert` for what the
            // material-only test destroyed.
            if slot.material != moving.material || slot.shape != moving.shape {
                continue;
            }
            let giving = moving.units.min(u32::MAX - slot.units);
            if giving > 0
                && let Ok(part) = moving.split(giving)
                && slot.merge(&part).is_err()
            {
                // Refused after the units were already out: put them back
                // rather than drop them. Belt and braces behind the guard
                // above, because this is a conservation law.
                let _ = moving.merge(&part);
            }
            if moving.is_empty() {
                break;
            }
        }
        if !moving.is_empty()
            && let Some(empty) = self.views[to].slots.iter_mut().find(|slot| slot.is_none())
        {
            *empty = Some(moving.clone());
            moving.units = 0;
        }

        // Whatever would not fit goes back where it came from, rather than
        // being quietly eaten.
        self.views[from].slots[index] = (!moving.is_empty()).then_some(moving);
        true
    }

    /// Swaps a hotbar slot with the off-hand.
    ///
    /// **A swap, not a move.** Whatever is in the off-hand comes back to the
    /// slot the player was holding, so the gesture is its own undo — pressing
    /// the key twice leaves the inventory exactly as it was, which is what
    /// makes it safe to press without looking.
    ///
    /// Returns whether anything moved. `false` for a slot that does not exist,
    /// or for a swap of two empty hands.
    pub fn swap_offhand(&mut self, view: &str, slot: usize) -> bool {
        let Some(at) = self.locate(view, slot) else {
            return false;
        };
        if slot == PLAYER_OFFHAND_SLOT || self.views[at].slots.len() <= PLAYER_OFFHAND_SLOT {
            return false;
        }
        if self.views[at].slots[slot].is_none()
            && self.views[at].slots[PLAYER_OFFHAND_SLOT].is_none()
        {
            return false;
        }
        self.views[at].slots.swap(slot, PLAYER_OFFHAND_SLOT);
        true
    }

    /// Shift-clicks within one view: between the hotbar band and the rest.
    ///
    /// **The gesture still means something with one view.** It used to move a
    /// stack from `player:main` to `player:hotbar` and back, which is what two
    /// views made it mean; with the hotbar a BAND of the player's own slots,
    /// the same gesture sends a stack out of the band or into it. That is what
    /// a player is asking for either way — "put this where the number keys can
    /// reach it", or "get it out of my way".
    ///
    /// `band` is how many slots at the front are the hotbar. Anything that will
    /// not fit stays where it was, which is what makes the gesture safe rather
    /// than lossy.
    pub fn stow(&mut self, view: &str, index: usize, band: usize) -> bool {
        let Some(at) = self.locate(view, index) else {
            return false;
        };
        let Some(mut moving) = self.views[at].slots[index].take() else {
            return false;
        };
        let slots = self.views[at].slots.len();
        let target: Vec<usize> = if index < band {
            (band..slots).collect()
        } else {
            (0..band.min(slots)).collect()
        };

        // Matching stacks first, so stowing twenty units onto a slot that
        // already holds some tops it up instead of taking a new slot.
        for slot in target.iter().copied() {
            let Some(stack) = &mut self.views[at].slots[slot] else {
                continue;
            };
            if stack.material != moving.material || stack.shape != moving.shape {
                continue;
            }
            let giving = moving.units.min(u32::MAX - stack.units);
            if giving > 0
                && let Ok(part) = moving.split(giving)
                && stack.merge(&part).is_err()
            {
                let _ = moving.merge(&part);
            }
            if moving.is_empty() {
                break;
            }
        }
        if !moving.is_empty()
            && let Some(empty) = target
                .iter()
                .copied()
                .find(|slot| self.views[at].slots[*slot].is_none())
        {
            self.views[at].slots[empty] = Some(moving.clone());
            moving.units = 0;
        }

        // Whatever would not fit goes back where it came from.
        self.views[at].slots[index] = (!moving.is_empty()).then_some(moving);
        true
    }

    /// Puts a stack into a view, filling matching stacks then empty slots.
    ///
    /// **Never lossy.** If the view has no room, it GROWS — a player digging
    /// their thirty-seventh material must not have it vanish because a screen
    /// shows thirty-six slots. How many a mod chooses to display is a question
    /// about its `item_grid`, not about what the player owns.
    ///
    /// Returns whether anything was added, which is what marks the view dirty.
    pub fn insert(&mut self, view: &str, mut stack: Stack) -> bool {
        if stack.is_empty() {
            return false;
        }
        let Some(at) = self.index_of(view) else {
            return false;
        };
        for slot in self.views[at].slots.iter_mut().flatten() {
            // **The shape is half of what makes two stacks the same stack.**
            // Material alone was the test until shaped stacks existed, and the
            // failure was silent and total: `split` takes the units out of the
            // incoming stack, `merge` refuses the mismatch, and the part it
            // refused is dropped on the floor. A player putting stairs into a
            // bag holding loose stone of the same material lost the stairs.
            if slot.material != stack.material || slot.shape != stack.shape {
                continue;
            }
            // **A slot holds one stack and no more.** What does not fit falls
            // through to the next matching slot, then to an empty one, then to
            // a slot the view grows for it.
            let room = slot.capacity().saturating_sub(slot.units);
            let giving = stack.units.min(room);
            if giving > 0
                && let Ok(part) = stack.split(giving)
                && slot.merge(&part).is_err()
            {
                let _ = stack.merge(&part);
            }
            if stack.is_empty() {
                return true;
            }
        }
        // A stack bigger than one slot holds is laid out over as many as it
        // needs, rather than being refused or quietly truncated.
        let cap = stack.capacity();
        while !stack.is_empty() {
            // Never fails: the amount asked for is the smaller of what is left
            // and one slot's worth.
            let Ok(part) = stack.split(stack.units.min(cap)) else {
                break;
            };
            if let Some(empty) = self.views[at].slots.iter_mut().find(|slot| slot.is_none()) {
                *empty = Some(part);
            } else {
                self.views[at].slots.push(Some(part));
            }
        }
        true
    }

    /// Takes up to `units` of a material out of a view, returning how many.
    ///
    /// Walks slots in order, so a player spending material empties the stack
    /// they can see first rather than one chosen by a hash.
    /// `detail` names WHICH stack, matched exactly, with `None` meaning a
    /// plain one. **Exactly, not "any"** — a recipe asking for stone must not
    /// melt down the named sword somebody left in the same view, and a mod
    /// that does want any reads the inventory and asks for each detail it
    /// finds. See [`super::Stack::detail`].
    pub fn take(
        &mut self,
        view: &str,
        material: MaterialId,
        shape: Option<super::Shape>,
        detail: Option<&str>,
        units: u32,
    ) -> u32 {
        let Some(at) = self.index_of(view) else {
            return 0;
        };
        let mut left = units;
        for slot in &mut self.views[at].slots {
            if left == 0 {
                break;
            }
            let Some(stack) = slot else { continue };
            // **The cut is part of what is being spent.** A player placing a
            // stair must not have it paid for out of their loose rubble, or the
            // stairs they crafted would sit in the inventory while the material
            // quietly drained away.
            if stack.material != material
                || stack.shape != shape
                || stack.detail.as_deref() != detail
            {
                continue;
            }
            let taking = stack.units.min(left);
            if let Ok(part) = stack.split(taking) {
                left -= part.units;
            }
            if stack.is_empty() {
                *slot = None;
            }
        }
        units - left
    }

    /// A view's contents as consolidated stacks, one per material.
    ///
    /// The shape the rest of the server already speaks: `InventoryUpdate` and
    /// the hotbar want "what do I have", not "where is it". Derived rather than
    /// stored, so there is one source of truth and it is the slots.
    #[must_use]
    pub fn consolidated(&self, view: &str) -> Vec<Stack> {
        let Some(view) = self.view(view) else {
            return Vec::new();
        };
        super::consolidate(view.slots.iter().flatten().cloned())
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
    // **Two things that cannot merge swap instead.** Material alone was the
    // test, so a held stair left-clicked onto loose stone of the same material
    // fell through to the merge, was refused, and NOTHING HAPPENED — which
    // from the window is a slot that ignores clicks. A different cut is a
    // different item, and a different item swaps.
    if held.material != there.material || held.shape != there.shape {
        // Swap. The hand takes what was there.
        return (Some(held), Some(there));
    }
    let mut merged = there;
    match merged.merge(&held) {
        Ok(()) => (Some(merged), None),
        // Overflow, or a mod's own difference: leave both alone rather than
        // lose what would not fit.
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
        // **Same shape as well as same material.** With material alone, a held
        // stair placed one unit at a time onto loose rubble of the same stone
        // vanished a unit per click: `merge` refused the shape and the unit
        // that had already been split out of the hand went nowhere. Found by
        // `no_run_of_clicks_changes_how_many_units_exist` the moment its
        // generator started producing shaped stacks.
        // **And the detail**, for the reason the shape is here: a held item a
        // mod says is different from the one in the slot must not merge, and
        // `merge` refusing after the unit has been split out of the hand is
        // where a unit goes missing.
        Some(mut there)
            if there.material == one.material
                && there.shape == one.shape
                && there.detail == one.detail =>
        {
            if there.merge(&one).is_err() {
                let _ = held.merge(&one);
            }
            Some(there)
        }
        // Nowhere to put one unit; nothing happens and the hand keeps
        // everything it had.
        Some(there) => {
            let _ = held.merge(&one);
            Some(there)
        }
    };
    (slot, (!held.is_empty()).then_some(held))
}

#[cfg(test)]
mod held_tests {
    use super::*;
    use crate::material::MaterialId;

    fn player() -> Slots {
        Slots::for_player_with(&[])
    }

    #[test]
    fn closing_a_screen_puts_what_is_in_hand_back() {
        // **Reported from the window as items vanishing.** The stack was never
        // lost — it is on the cursor, on the server — but a cursor has no
        // picture once the screen is gone, and it would now sit there across a
        // save as well.
        let mut slots = player();
        slots.views[0].slots[3] = Stack::new(MaterialId(2), 27);
        assert!(slots.left_click(PLAYER_MAIN, 3), "picked it up");
        assert!(slots.grab.held.is_some(), "it is in hand");
        assert_eq!(slots.views[0].slots[3], None, "and out of its slot");

        assert!(slots.return_held(PLAYER_MAIN));
        assert_eq!(slots.grab.held, None, "the hand is empty again");
        let total: u32 = slots.views[0]
            .slots
            .iter()
            .flatten()
            .map(|stack| stack.units)
            .sum();
        assert_eq!(total, 27, "and the units are back in the inventory");
    }

    #[test]
    fn returning_an_empty_hand_changes_nothing() {
        let mut slots = player();
        assert!(!slots.return_held(PLAYER_MAIN));
    }

    #[test]
    fn a_stack_with_nowhere_to_go_stays_in_hand_rather_than_vanishing() {
        // Destroying it would be worse than leaving it: the next screen the
        // player opens can still show it.
        let mut slots = player();
        slots.grab.held = Stack::new(MaterialId(2), 27);
        assert!(!slots.return_held("nobody:such-view"));
        assert!(
            slots.grab.held.is_some(),
            "an item with nowhere to go was destroyed"
        );
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_offhand_swap_is_its_own_undo() {
        // Pressing the key twice must leave the inventory exactly as it was, or
        // it is not a gesture anybody can use without looking.
        let mut inv = Slots::for_player();
        inv.views[0].slots[0] = Some(Stack::new(STONE, 20).expect("stack"));
        inv.views[0].slots[PLAYER_OFFHAND_SLOT] = Some(Stack::new(STONE, 7).expect("stack"));
        let before = inv.clone();

        assert!(inv.swap_offhand(PLAYER_MAIN, 0));
        assert_eq!(
            at(&inv, PLAYER_MAIN, 0).expect("swapped").units,
            7,
            "the off-hand's stack did not come to the hand"
        );
        assert!(inv.swap_offhand(PLAYER_MAIN, 0));
        assert_eq!(inv, before, "two presses did not put it back");
    }

    #[test]
    fn swapping_an_empty_hand_with_an_empty_offhand_does_nothing() {
        // So a key pressed by accident does not mark the inventory dirty and
        // send an update saying nothing changed.
        let mut inv = Slots::for_player();
        assert!(!inv.swap_offhand(PLAYER_MAIN, 3));
        // And the off-hand cannot be swapped with itself.
        inv.views[0].slots[PLAYER_OFFHAND_SLOT] = Some(Stack::new(STONE, 5).expect("stack"));
        assert!(!inv.swap_offhand(PLAYER_MAIN, PLAYER_OFFHAND_SLOT));
        assert_eq!(inv.total_units(), 5, "a refused swap changed something");
    }

    #[test]
    fn shift_clicking_moves_a_stack_between_the_hotbar_band_and_the_rest() {
        // The gesture, with one view: out of the band, and back into it. What
        // a player means either way is "put this where the keys can reach it"
        // or "get it out of my way", and both are the same click.
        let mut inv = Slots::for_player();
        assert!(inv.insert(PLAYER_MAIN, Stack::new(STONE, 50).expect("stack")));

        assert!(inv.stow(PLAYER_MAIN, 0, PLAYER_HOTBAR_SLOTS));
        assert!(
            at(&inv, PLAYER_MAIN, 0).is_none(),
            "the stack did not leave the hotbar"
        );
        let landed = (PLAYER_HOTBAR_SLOTS..PLAYER_MAIN_SLOTS)
            .find(|slot| at(&inv, PLAYER_MAIN, *slot).is_some())
            .expect("it went somewhere outside the band");
        assert_eq!(inv.total_units(), 50, "stowing changed how much there was");

        assert!(inv.stow(PLAYER_MAIN, landed, PLAYER_HOTBAR_SLOTS));
        assert!(
            at(&inv, PLAYER_MAIN, 0).is_some(),
            "it did not come back into the band"
        );
        assert_eq!(inv.total_units(), 50);
    }

    #[test]
    fn stowing_tops_up_a_matching_stack_before_taking_a_new_slot() {
        let mut inv = Slots::for_player();
        // Slot 0 is in the band; slot 9 is the first outside it.
        inv.views[0].slots[0] = Some(Stack::new(STONE, 20).expect("stack"));
        inv.views[0].slots[9] = Some(Stack::new(STONE, 5).expect("stack"));

        assert!(inv.stow(PLAYER_MAIN, 0, PLAYER_HOTBAR_SLOTS));
        assert_eq!(
            at(&inv, PLAYER_MAIN, 9).expect("topped up").units,
            25,
            "the matching stack outside the band should have taken it"
        );
        assert_eq!(inv.total_units(), 25);
    }

    #[test]
    fn a_cut_and_loose_material_of_one_stone_swap_rather_than_stall() {
        use super::super::Shape;

        let shape = Shape::new(0b101).expect("two cells");
        let mut inv = Slots {
            views: vec![View {
                name: PLAYER_MAIN.to_owned(),
                slots: vec![Some(Stack::new(STONE, 50).expect("loose"))],
            }],
            grab: Grab {
                held: Some(Stack::shaped(STONE, shape, 3).expect("cut")),
            },
        };

        assert!(inv.left_click(PLAYER_MAIN, 0));
        assert_eq!(
            inv.view(PLAYER_MAIN).expect("view").slots[0]
                .as_ref()
                .expect("the slot took what was held")
                .shape,
            Some(shape),
            "the cut went into the slot"
        );
        assert_eq!(
            inv.grab.held.expect("the hand took what was there").shape,
            None,
            "the loose material came back to the hand"
        );
    }

    #[test]
    fn placing_one_cut_onto_loose_material_of_the_same_stone_loses_nothing() {
        use super::super::Shape;

        // The unit that used to disappear: `place_one` split one out of the
        // hand, `merge` refused the shape, and nobody was holding it.
        let shape = Shape::new(0b101).expect("two cells");
        let mut inv = Slots {
            views: vec![View {
                name: PLAYER_MAIN.to_owned(),
                slots: vec![Some(Stack::new(STONE, 50).expect("loose"))],
            }],
            grab: Grab {
                held: Some(Stack::shaped(STONE, shape, 3).expect("cut")),
            },
        };
        let before = inv.total_units();

        assert!(inv.right_click(PLAYER_MAIN, 0));
        assert_eq!(before, inv.total_units(), "a unit went missing");
    }

    #[test]
    fn spending_a_cut_does_not_drain_the_loose_material_beside_it() {
        use super::super::Shape;

        // **The conservation hole shapes could have opened.** A player placing
        // a stair must spend the stairs they crafted; paying for it out of the
        // rubble in the next slot would leave the stairs sitting there while
        // the material quietly went away.
        let shape = Shape::new(0b101).expect("two cells");
        let mut inv = Slots {
            views: vec![View {
                name: PLAYER_MAIN.to_owned(),
                slots: vec![
                    Some(Stack::new(STONE, 50).expect("loose")),
                    Some(Stack::shaped(STONE, shape, 3).expect("cut")),
                ],
            }],
            grab: Grab::default(),
        };

        // Taking the cut takes it from the cut slot only.
        assert_eq!(
            inv.take(PLAYER_MAIN, STONE, Some(shape), None, shape.cells()),
            shape.cells()
        );
        assert_eq!(
            inv.views[0].slots[0]
                .as_ref()
                .expect("loose survives")
                .units,
            50,
            "the loose material was spent on a shaped placement"
        );
        assert_eq!(
            inv.views[0].slots[1]
                .as_ref()
                .expect("cut survives")
                .count(),
            2
        );

        // And loose takes from loose, not from the cut.
        assert_eq!(inv.take(PLAYER_MAIN, STONE, None, None, 50), 50);
        assert!(inv.views[0].slots[0].is_none());
        assert_eq!(
            inv.views[0].slots[1]
                .as_ref()
                .expect("cut survives")
                .count(),
            2
        );

        // Asking for a cut nobody holds takes nothing rather than falling back.
        let other = Shape::new(0b11).expect("another cut");
        assert_eq!(inv.take(PLAYER_MAIN, STONE, Some(other), None, 2), 0);
    }

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
        slots.view(view).and_then(|v| v.slots[index].clone())
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
        use crate::inventory::Shape;

        fn any_stack() -> impl Strategy<Value = Option<Stack>> {
            prop_oneof![
                1 => Just(None),
                4 => (2u16..6, 1u32..200).prop_map(|(m, units)| Stack::new(MaterialId(m), units)),
                // **Shaped stacks of the SAME materials as the loose ones**,
                // so the generator actually produces the collision that
                // matters: a stair and loose rubble of one stone, which merge
                // has to refuse and everything around it has to survive.
                2 => (2u16..6, 1u32..0x07ff_ffff, 1u32..8).prop_map(|(m, mask, count)| {
                    Shape::new(mask).and_then(|shape| Stack::shaped(MaterialId(m), shape, count))
                }),
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

            /// **What a mod does to an inventory conserves units too.**
            ///
            /// Clicks are the player's route in; `insert` and `take` are the
            /// mod API's, and they are the ones that carry a shape. This
            /// caught `insert` destroying material outright: it split the
            /// units out of the incoming stack, `merge` refused the shape
            /// mismatch, and the part it refused was dropped.
            #[test]
            fn inserting_and_taking_conserves_units(
                mut slots in any_slots(),
                incoming in prop::collection::vec(any_stack(), 0..8),
                takes in prop::collection::vec((2u16..6, 1u32..300), 0..8),
            ) {
                let mut expected = slots.total_units();
                for stack in incoming.into_iter().flatten() {
                    expected += u64::from(stack.units);
                    slots.insert("player:main", stack);
                    prop_assert_eq!(
                        slots.total_units(),
                        expected,
                        "inserting a stack lost or invented units"
                    );
                }
                for (material, units) in takes {
                    let took = slots.take("player:main", MaterialId(material), None, None, units);
                    expected -= u64::from(took);
                    prop_assert_eq!(
                        slots.total_units(),
                        expected,
                        "taking reported a different amount than it removed"
                    );
                }
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
    fn a_fresh_player_has_room_before_they_have_anything() {
        // **Reported from the window: "items do not seem to display in my
        // inventory yet".** `player:main` started at zero slots and grew, so a
        // player who had dug one thing had a one-slot view — and a dialog
        // drawing a grid over it showed empty boxes for slots that did not
        // exist, which a click correctly does nothing to.
        let inv = Slots::for_player();
        assert_eq!(
            inv.view(PLAYER_MAIN).map(|view| view.slots.len()),
            Some(PLAYER_MAIN_SLOTS)
        );
        // One view, not two: the hotbar is a band inside this one.
        assert_eq!(
            inv.views.len(),
            1,
            "a second view is one a player has to shuffle things into"
        );
        assert_eq!(inv.total_units(), 0, "room is not contents");
    }

    #[test]
    fn a_starting_size_still_does_not_cap_what_a_player_may_own() {
        // The reason it started at zero was that growth must not be capped.
        // Starting with room does not change that, and this is the test that
        // says so.
        let mut inv = Slots::for_player();
        for material in 0..(PLAYER_MAIN_SLOTS as u16 + 5) {
            assert!(
                inv.insert(
                    PLAYER_MAIN,
                    Stack::new(MaterialId(material + 2), 27).expect("stack")
                ),
                "material {material} should fit"
            );
        }
        let held = inv.view(PLAYER_MAIN).expect("main").slots.len();
        assert!(
            held > PLAYER_MAIN_SLOTS,
            "the view should have grown past its starting size, got {held}"
        );
    }

    #[test]
    fn a_slot_holds_ninety_of_a_thing_and_the_rest_spills_over() {
        // Asked for from the window: "let's make stacks 90 blocks". Counted in
        // THINGS — ninety blocks of loose stone and ninety stairs are both one
        // stack, so the cap in units differs and the count a player sees does
        // not.
        let mut inv = Slots {
            views: vec![View::empty("player:main", 4)],
            grab: Grab::default(),
        };
        let whole = crate::inventory::ITEMS_PER_STACK * UNITS_PER_BLOCK;
        assert!(inv.insert(
            "player:main",
            Stack::new(MaterialId(3), whole + UNITS_PER_BLOCK).expect("stack")
        ));
        let slots: Vec<u32> = inv
            .view("player:main")
            .expect("main")
            .slots
            .iter()
            .flatten()
            .map(|stack| stack.units)
            .collect();
        assert_eq!(
            slots,
            vec![whole, UNITS_PER_BLOCK],
            "ninety-one blocks should be a full stack and one block over"
        );
        assert_eq!(
            inv.total_units(),
            u64::from(whole + UNITS_PER_BLOCK),
            "capping a slot lost material"
        );
    }

    #[test]
    fn a_stack_of_stairs_is_ninety_stairs_and_not_ninety_blocks_of_them() {
        let cut = crate::inventory::Shape::new(0b11111).expect("a cut");
        let one = cut.cells();
        let mut inv = Slots {
            views: vec![View::empty("player:main", 4)],
            grab: Grab::default(),
        };
        assert!(
            inv.insert(
                "player:main",
                Stack::shaped(MaterialId(3), cut, crate::inventory::ITEMS_PER_STACK + 1)
                    .expect("stack")
            )
        );
        let slots: Vec<u32> = inv
            .view("player:main")
            .expect("main")
            .slots
            .iter()
            .flatten()
            .map(|stack| stack.units)
            .collect();
        assert_eq!(
            slots,
            vec![crate::inventory::ITEMS_PER_STACK * one, one],
            "a stack of stairs should hold ninety stairs, whatever a stair costs"
        );
    }

    #[test]
    fn inserting_more_than_fits_grows_the_view_rather_than_losing_it() {
        // **The regression this rules out.** A consolidated inventory had no
        // size at all, so moving to slots could silently cap what a player can
        // own. How many slots a mod DISPLAYS is a question about its
        // `item_grid`; what the player owns is not.
        let mut inv = Slots {
            views: vec![View::empty("player:main", 2)],
            grab: Grab::default(),
        };
        for material in 2..8u16 {
            assert!(inv.insert(
                "player:main",
                Stack::new(MaterialId(material), 10).expect("stack")
            ));
        }
        assert_eq!(
            inv.total_units(),
            60,
            "a stack was lost when the view filled"
        );
        assert_eq!(
            inv.view("player:main").expect("view").slots.len(),
            6,
            "the view should have grown"
        );
    }

    #[test]
    fn inserting_tops_up_a_matching_stack_before_taking_a_slot() {
        let mut inv = Slots {
            views: vec![View {
                name: "player:main".to_owned(),
                slots: vec![stack(STONE, 5), None],
            }],
            grab: Grab::default(),
        };
        inv.insert("player:main", Stack::new(STONE, 7).expect("stack"));
        assert_eq!(at(&inv, "player:main", 0).expect("topped up").units, 12);
        assert!(
            at(&inv, "player:main", 1).is_none(),
            "an empty slot was used anyway"
        );
    }

    #[test]
    fn taking_spends_slots_in_order_and_reports_what_it_got() {
        let mut inv = Slots {
            views: vec![View {
                name: "player:main".to_owned(),
                slots: vec![stack(STONE, 5), stack(DIRT, 9), stack(STONE, 4)],
            }],
            grab: Grab::default(),
        };
        // Spanning two slots, first one emptied.
        assert_eq!(inv.take("player:main", STONE, None, None, 7), 7);
        assert!(
            at(&inv, "player:main", 0).is_none(),
            "an emptied slot must be None"
        );
        assert_eq!(
            at(&inv, "player:main", 2).as_ref().expect("partial").units,
            2
        );
        assert_eq!(inv.total_units(), 11);

        // Asking for more than there is takes what there is and says so.
        assert_eq!(inv.take("player:main", STONE, None, None, 99), 2);
        assert_eq!(inv.take("player:main", STONE, None, None, 1), 0);
        // A material nobody has, and a view nobody has.
        assert_eq!(inv.take("player:main", MaterialId(77), None, None, 5), 0);
        assert_eq!(inv.take("nosuch:view", DIRT, None, None, 5), 0);
        assert_eq!(inv.total_units(), 9, "only the dirt should be left");
    }

    #[test]
    fn consolidating_gives_one_stack_per_material() {
        // The shape the rest of the server speaks. Derived from the slots
        // rather than stored beside them, so there is one source of truth.
        let inv = Slots {
            views: vec![View {
                name: "player:main".to_owned(),
                slots: vec![stack(STONE, 5), stack(DIRT, 9), None, stack(STONE, 4)],
            }],
            grab: Grab::default(),
        };
        let held = inv.consolidated("player:main");
        assert_eq!(held.len(), 2, "{held:?}");
        assert_eq!(
            held.iter().map(|s| u64::from(s.units)).sum::<u64>(),
            18,
            "consolidating changed the total"
        );
        assert!(inv.consolidated("nosuch:view").is_empty());
    }

    #[test]
    fn a_right_click_splits_forty_units_into_twenty_and_twenty() {
        // **Criterion 4, in one assertion.** Charter rule 5: the halving is on
        // UNITS. 40 units is one block and thirteen nodes; splitting it gives
        // 20 and 20 — not "one block each" and not "one block and a bit",
        // either of which would invent or destroy units.
        let mut inv = slots(vec![stack(STONE, 40), None]);
        assert!(inv.right_click("player:main", 0));

        let held = inv.grab.held.clone().expect("something in hand");
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
        assert_eq!(inv.grab.held.as_ref().expect("hand").units, 21);
        assert_eq!(at(&inv, "player:main", 0).expect("behind").units, 20);

        // One unit cannot be halved: the hand takes it and the slot empties.
        let mut inv = slots(vec![stack(STONE, 1), None]);
        inv.right_click("player:main", 0);
        assert_eq!(inv.grab.held.as_ref().expect("hand").units, 1);
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
        assert_eq!(
            inv.grab.held.as_ref().expect("hand").units,
            UNITS_PER_BLOCK - 1
        );

        // Onto a matching stack it tops it up.
        inv.right_click("player:main", 0);
        assert_eq!(at(&inv, "player:main", 0).expect("placed").units, 2);

        // Onto a DIFFERENT material nothing happens, and nothing is lost.
        let before = inv.total_units();
        let mut inv2 = slots(vec![stack(DIRT, 5), None]);
        inv2.grab.held = stack(STONE, 10);
        inv2.right_click("player:main", 0);
        assert_eq!(at(&inv2, "player:main", 0).expect("still dirt").units, 5);
        assert_eq!(inv2.grab.held.as_ref().expect("hand").units, 10);
        assert_eq!(inv.total_units(), before);
    }

    #[test]
    fn a_left_click_takes_places_merges_and_swaps() {
        // Take.
        let mut inv = slots(vec![stack(STONE, 30), None]);
        inv.left_click("player:main", 0);
        assert_eq!(inv.grab.held.as_ref().expect("hand").units, 30);
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
        assert_eq!(inv.grab.held.as_ref().expect("hand").material, STONE);
        assert_eq!(inv.grab.held.as_ref().expect("hand").units, 35);
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
