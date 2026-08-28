// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Inventories anchored to the world: chests, furnaces, hoppers.
//!
//! # Lent to one player at a time, and that is not a shortcut
//!
//! A click on a slot moves a stack between that slot and the player's CURSOR,
//! and the cursor lives on the player. So a container is opened by lending its
//! view into that player's own [`Slots`] — after which every mechanism the
//! engine already has works on it unchanged: left click, right click, shift
//! click, the view updates that go to the client, `game.give` and `game.take`
//! by view name. Closing takes it back.
//!
//! **Lending to two players at once would duplicate items.** Each would get a
//! copy, each would click their own, and whichever closed second would write
//! theirs over the other's. So a container has one holder, and opening one
//! somebody else is in is refused — a mod can say "somebody is using that",
//! which is a sentence a player understands. Several viewers at once is
//! possible later by moving the cursor rather than the container, and it does
//! not change the mod API.
//!
//! # What the engine owns and what it does not
//!
//! The slots, their stacking, their conservation and their persistence. Not
//! where the container is, not what may go in it, not who may open it —
//! charter rule 1 puts every one of those in a mod, which is why this is keyed
//! by a name the mod chooses rather than by a block position the engine would
//! have to understand.

use std::collections::BTreeMap;

use tiamot_core::PlayerUuid;
use tiamot_core::inventory::{Slots, View};

/// One container, and who has it open.
#[derive(Debug, Clone)]
pub struct Container {
    /// Its contents, when nobody has it open.
    ///
    /// `None` while it is lent: the view is in that player's own [`Slots`] and
    /// there must be exactly one copy of it anywhere.
    pub view: Option<View>,
    /// Who has it open, if anybody.
    pub holder: Option<PlayerUuid>,
    /// How many slots it has, kept so a container that is lent can still say.
    pub slots: usize,
}

/// Every container in the world.
#[derive(Debug, Default)]
pub struct Containers {
    by_name: BTreeMap<String, Container>,
    /// Names whose rows want writing.
    dirty: std::collections::BTreeSet<String>,
    /// Names whose rows want deleting.
    gone: std::collections::BTreeSet<String>,
}

impl Containers {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Puts a container back as the world had it.
    pub fn restore(&mut self, name: String, view: View) {
        let slots = view.slots.len();
        self.by_name.insert(
            name,
            Container {
                view: Some(view),
                holder: None,
                slots,
            },
        );
    }

    /// Makes a container if there is not one, and reports whether it is new.
    ///
    /// The size applies to a container being MADE. An existing one keeps the
    /// size it has, because growing or shrinking somebody's chest under them is
    /// a decision a mod should have to make out loud — see
    /// [`Containers::resize`].
    pub fn ensure(&mut self, name: &str, slots: usize) -> bool {
        if self.by_name.contains_key(name) {
            return false;
        }
        self.by_name.insert(
            name.to_owned(),
            Container {
                view: Some(View::empty(name, slots)),
                holder: None,
                slots,
            },
        );
        self.dirty.insert(name.to_owned());
        true
    }

    /// Changes how many slots a container has, keeping what still fits.
    ///
    /// Returns what did not fit, so a mod can drop it on the floor rather than
    /// have the engine destroy it. Refused — and everything returned — while
    /// somebody has it open.
    pub fn resize(&mut self, name: &str, slots: usize) -> Vec<tiamot_core::inventory::Stack> {
        let Some(container) = self.by_name.get_mut(name) else {
            return Vec::new();
        };
        let Some(view) = container.view.as_mut() else {
            return Vec::new();
        };
        let mut spilled = Vec::new();
        while view.slots.len() > slots {
            if let Some(Some(stack)) = view.slots.pop() {
                spilled.push(stack);
            }
        }
        view.slots.resize(slots, None);
        container.slots = slots;
        self.dirty.insert(name.to_owned());
        spilled
    }

    /// Lends a container to a player, putting its view into their own slots.
    ///
    /// Returns whether it opened. `false` means it does not exist, or somebody
    /// else has it — which is the case a mod tells the player about.
    pub fn open(&mut self, name: &str, uuid: PlayerUuid, into: &mut Slots) -> bool {
        let Some(container) = self.by_name.get_mut(name) else {
            return false;
        };
        if container.holder.is_some_and(|holder| holder != uuid) {
            return false;
        }
        // Already open for this player. Not an error: a mod redrawing its
        // screen should not have to remember whether it opened one.
        if container.holder == Some(uuid) {
            return true;
        }
        let Some(view) = container.view.take() else {
            return false;
        };
        container.holder = Some(uuid);
        into.views.push(view);
        true
    }

    /// Takes a container back out of a player's slots.
    ///
    /// Returns whether anything came back. Called when the screen closes, when
    /// the player disconnects, and when a mod asks — and it must happen on
    /// every one of those, or the container is lent to somebody who is not
    /// there and nobody can open it again.
    pub fn close(&mut self, name: &str, uuid: PlayerUuid, from: &mut Slots) -> bool {
        let Some(container) = self.by_name.get_mut(name) else {
            return false;
        };
        if container.holder != Some(uuid) {
            return false;
        }
        let Some(at) = from.views.iter().position(|view| view.name == name) else {
            // Lent, and not there. The holder is cleared anyway: leaving it set
            // would strand the container for the rest of the session.
            container.holder = None;
            return false;
        };
        let view = from.views.remove(at);
        container.slots = view.slots.len();
        container.view = Some(view);
        container.holder = None;
        self.dirty.insert(name.to_owned());
        true
    }

    /// Takes back everything one player has open, for a disconnect.
    ///
    /// Returns the names, so a caller can say what it put away.
    pub fn close_all(&mut self, uuid: PlayerUuid, from: &mut Slots) -> Vec<String> {
        let open: Vec<String> = self
            .by_name
            .iter()
            .filter(|(_, container)| container.holder == Some(uuid))
            .map(|(name, _)| name.clone())
            .collect();
        for name in &open {
            self.close(name, uuid, from);
        }
        open
    }

    /// What is in a container, when nobody has it open.
    ///
    /// `None` while it is lent — a mod reading a chest somebody is standing in
    /// would be reading a copy that is about to be replaced, and answering
    /// with one would be worse than answering with nothing.
    #[must_use]
    pub fn contents(&self, name: &str) -> Option<&View> {
        self.by_name.get(name)?.view.as_ref()
    }

    /// The same, to write into. Marks the container for saving.
    pub fn contents_mut(&mut self, name: &str) -> Option<&mut View> {
        let container = self.by_name.get_mut(name)?;
        let view = container.view.as_mut()?;
        self.dirty.insert(name.to_owned());
        Some(view)
    }

    /// Whether a container exists at all.
    #[must_use]
    pub fn exists(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    /// Who has it open, if anybody.
    #[must_use]
    pub fn holder(&self, name: &str) -> Option<PlayerUuid> {
        self.by_name.get(name)?.holder
    }

    /// Removes a container and returns what was in it.
    ///
    /// **Nothing is destroyed here.** The contents come back so the mod that
    /// broke the block can drop them; an engine that emptied a chest into
    /// nowhere would be a conservation hole (charter rule 5) on a path a
    /// player caused and could see.
    ///
    /// Refuses while somebody has it open, because those items are in another
    /// player's screen and taking them would be taking them out of their hands.
    pub fn remove(&mut self, name: &str) -> Option<Vec<tiamot_core::inventory::Stack>> {
        if self.by_name.get(name)?.holder.is_some() {
            return None;
        }
        let container = self.by_name.remove(name)?;
        self.dirty.remove(name);
        self.gone.insert(name.to_owned());
        Some(
            container
                .view
                .map(|view| view.slots.into_iter().flatten().collect())
                .unwrap_or_default(),
        )
    }

    /// Names whose rows want writing, and which want deleting.
    ///
    /// Drained, so a caller that saves them need not remember.
    pub fn take_pending(&mut self) -> (Vec<String>, Vec<String>) {
        (
            std::mem::take(&mut self.dirty).into_iter().collect(),
            std::mem::take(&mut self.gone).into_iter().collect(),
        )
    }

    /// Marks every container for writing, for a full save.
    pub fn mark_all(&mut self) {
        let names: Vec<String> = self.by_name.keys().cloned().collect();
        self.dirty.extend(names);
    }

    /// How many containers the world holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    /// Whether there are none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

/// The mod-facing handle on the world's containers.
///
/// Holds the store and the players' inventories together, because opening a
/// container moves a view between them and neither half makes sense alone.
pub struct Shared {
    store: std::sync::Arc<std::sync::Mutex<Containers>>,
    endpoint: std::sync::Arc<crate::transport::Shared>,
}

impl Shared {
    /// Wraps the store the simulation thread owns.
    #[must_use]
    pub const fn new(
        store: std::sync::Arc<std::sync::Mutex<Containers>>,
        endpoint: std::sync::Arc<crate::transport::Shared>,
    ) -> Self {
        Self { store, endpoint }
    }
}

impl tiamot_core::inventory::Containers for Shared {
    fn ensure(&self, name: &str, slots: usize) -> bool {
        self.store
            .lock()
            .is_ok_and(|mut store| store.ensure(name, slots))
    }

    fn open(&self, name: &str, player: [u8; 32]) -> bool {
        let uuid = PlayerUuid::from_bytes(player);
        let Ok(mut store) = self.store.lock() else {
            return false;
        };
        let opened = self
            .endpoint
            .with_slots(&uuid, |slots| store.open(name, uuid, slots))
            .unwrap_or(false);
        if opened {
            // **Told at once.** The screen the mod is about to draw names this
            // view, and a grid whose contents have not arrived draws as empty
            // — which reads as a chest that lost everything in it.
            self.endpoint.mark_inventory_dirty(&uuid);
        }
        opened
    }

    fn close(&self, name: &str, player: [u8; 32]) -> bool {
        let uuid = PlayerUuid::from_bytes(player);
        let Ok(mut store) = self.store.lock() else {
            return false;
        };
        let closed = self
            .endpoint
            .with_slots(&uuid, |slots| store.close(name, uuid, slots))
            .unwrap_or(false);
        if closed {
            self.endpoint.mark_inventory_dirty(&uuid);
        }
        closed
    }

    fn contents(&self, name: &str) -> Vec<tiamot_core::inventory::Stack> {
        self.store
            .lock()
            .ok()
            .and_then(|store| {
                store
                    .contents(name)
                    .map(|view| view.slots.iter().flatten().cloned().collect())
            })
            .unwrap_or_default()
    }

    fn remove(&self, name: &str) -> Vec<tiamot_core::inventory::Stack> {
        self.store
            .lock()
            .ok()
            .and_then(|mut store| store.remove(name))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiamot_core::MaterialId;
    use tiamot_core::inventory::{Grab, Stack};

    fn player(byte: u8) -> PlayerUuid {
        PlayerUuid::from_bytes([byte; 32])
    }

    fn slots() -> Slots {
        Slots {
            views: vec![View::empty("player:main", 4)],
            grab: Grab::default(),
        }
    }

    #[test]
    fn opening_a_container_moves_its_view_into_the_players_own_slots() {
        // **The whole design.** A click moves a stack between a slot and the
        // player's cursor, and the cursor is on the player — so the container
        // goes to where the cursor is and every mechanism the engine already
        // has works on it unchanged.
        let mut store = Containers::new();
        assert!(store.ensure("core_chest:at:1,2,3", 9));
        let mut mine = slots();

        assert!(store.open("core_chest:at:1,2,3", player(1), &mut mine));
        assert_eq!(
            mine.view("core_chest:at:1,2,3")
                .map(|view| view.slots.len()),
            Some(9),
            "the container is not in the player's slots"
        );
        assert!(
            store.contents("core_chest:at:1,2,3").is_none(),
            "there are two copies of the container"
        );
    }

    #[test]
    fn a_container_somebody_else_has_open_does_not_open_twice() {
        // **Lending it twice would duplicate items**: each player would get a
        // copy, click their own, and whoever closed second would write theirs
        // over the other's.
        let mut store = Containers::new();
        store.ensure("chest", 9);
        let mut ada = slots();
        let mut bert = slots();

        assert!(store.open("chest", player(1), &mut ada));
        assert!(
            !store.open("chest", player(2), &mut bert),
            "two players opened one container"
        );
        assert!(bert.view("chest").is_none(), "the second got a copy anyway");
    }

    #[test]
    fn what_a_player_put_in_comes_back_when_they_close_it() {
        let mut store = Containers::new();
        store.ensure("chest", 4);
        let mut mine = slots();
        assert!(store.open("chest", player(1), &mut mine));
        assert!(mine.insert("chest", Stack::new(MaterialId(2), 30).expect("stack")));

        assert!(store.close("chest", player(1), &mut mine));
        assert!(mine.view("chest").is_none(), "it stayed in the player");
        let total: u32 = store
            .contents("chest")
            .expect("back in the world")
            .slots
            .iter()
            .flatten()
            .map(|stack| stack.units)
            .sum();
        assert_eq!(total, 30, "what was put in did not come back");

        // And now somebody else can open it.
        let mut theirs = slots();
        assert!(store.open("chest", player(2), &mut theirs));
    }

    #[test]
    fn a_disconnect_puts_back_everything_that_player_had_open() {
        // A container lent to somebody who is not there is a container nobody
        // can ever open again.
        let mut store = Containers::new();
        store.ensure("one", 4);
        store.ensure("two", 4);
        let mut mine = slots();
        assert!(store.open("one", player(1), &mut mine));
        assert!(store.open("two", player(1), &mut mine));

        let closed = store.close_all(player(1), &mut mine);
        assert_eq!(closed, ["one".to_owned(), "two".to_owned()]);
        assert!(store.contents("one").is_some() && store.contents("two").is_some());
        assert!(store.holder("one").is_none() && store.holder("two").is_none());
    }

    #[test]
    fn breaking_a_container_hands_back_what_was_in_it() {
        // Charter rule 5 on a path a player caused and can see: the engine
        // must not empty a chest into nowhere.
        let mut store = Containers::new();
        store.ensure("chest", 4);
        store.contents_mut("chest").expect("just made").slots[0] = Stack::new(MaterialId(2), 30);

        let spilled = store.remove("chest").expect("it existed");
        assert_eq!(spilled.len(), 1);
        assert_eq!(spilled[0].units, 30);
        assert!(!store.exists("chest"));
    }

    #[test]
    fn a_container_somebody_is_looking_at_cannot_be_broken_out_from_under_them() {
        let mut store = Containers::new();
        store.ensure("chest", 4);
        let mut mine = slots();
        assert!(store.open("chest", player(1), &mut mine));
        assert!(
            store.remove("chest").is_none(),
            "a chest was taken out of somebody's open screen"
        );
    }

    #[test]
    fn shrinking_a_container_hands_back_what_no_longer_fits() {
        let mut store = Containers::new();
        store.ensure("chest", 4);
        store.contents_mut("chest").expect("just made").slots[3] = Stack::new(MaterialId(2), 5);

        let spilled = store.resize("chest", 2);
        assert_eq!(
            spilled.len(),
            1,
            "the last row was destroyed rather than returned"
        );
        assert_eq!(store.contents("chest").expect("still there").slots.len(), 2);
    }
}
