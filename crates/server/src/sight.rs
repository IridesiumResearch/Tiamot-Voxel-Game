// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Lending the world to the mods, for exactly as long as they are running.
//!
//! # The problem this solves
//!
//! Every other mod-facing store — lighting, fluid, entities, storage — sits
//! behind its own `Arc<RwLock<…>>` for the whole run, so the VM can hold a
//! handle to it from freeze onwards. The world cannot. The tick thread owns
//! [`World`] by value and holds it *mutably* through chunk generation, through
//! every edit it applies, and through the lighting and fluid passes, so there
//! is no handle that is safe to hold at an arbitrary moment.
//!
//! Putting `World` behind a lock anyway is where the obvious version of this
//! goes wrong. Chunk generation runs mod callbacks *while* the world is
//! borrowed, so a mod calling into the world from `on_generate` would take the
//! same lock the tick is already holding — a self-deadlock on the simulation
//! thread, from Lua, on a mod author's mistake. Defusing that with `try_read`
//! turns the deadlock into a silent wrong answer, which is worse.
//!
//! # What this does instead
//!
//! The tick **moves** the world into a slot for the part of the tick that runs
//! mod callbacks, and moves it back out afterwards. Between those two moments
//! the tick has no world at all — the borrow checker enforces that, because the
//! value is gone — so there is nothing for a mod's read to contend with, and the
//! lock behind the slot is only ever taken by one side at a time.
//!
//! It also makes the window a fact about the code rather than a convention.
//! A mod that reads the world outside it gets [`Sighting::Unavailable`], which
//! is exactly true: at that moment the engine is mid-edit and there is no
//! consistent world to answer from.
//!
//! # Cost
//!
//! Two moves of a `World` per lend — a memcpy of a handful of pointers and
//! lengths, not of any chunk — and one uncontended mutex acquisition per mod
//! call. The chunks themselves never move.

use std::sync::{Arc, Mutex};

use tiamot_core::sight::{Access, Sighting};

use crate::world::World;

/// The slot the world is lent through.
///
/// Held by the tick, which is the only thing that may put a world in or take
/// one out. Hand [`Self::handle`] to the VM.
pub struct Lease {
    slot: Arc<Mutex<Option<World>>>,
}

impl Default for Lease {
    fn default() -> Self {
        Self::new()
    }
}

impl Lease {
    /// An empty slot.
    #[must_use]
    pub fn new() -> Self {
        Self {
            slot: Arc::new(Mutex::new(None)),
        }
    }

    /// The mod-facing handle. Answers [`Sighting::Unavailable`] whenever the
    /// slot is empty, which is every moment outside a lend.
    #[must_use]
    pub fn handle(&self) -> Shared {
        Shared {
            slot: Arc::clone(&self.slot),
        }
    }

    /// Runs `body` with the world lent to the mods, and gives it back.
    ///
    /// The world is moved in and out rather than borrowed, so the caller
    /// **cannot** touch it inside `body` — that is the point, and the compiler
    /// is what enforces it. Anything the tick needs from the world goes before
    /// or after this call, never inside.
    ///
    /// # Panics
    ///
    /// If the slot's lock is poisoned, or if the world is not in it afterwards.
    /// Both mean the simulation thread has already panicked somewhere inside
    /// `body`, at which point there is no world to carry on with — the tick is
    /// over either way, and a lost world is worth saying so about.
    pub fn lending<T>(&self, world: World, body: impl FnOnce() -> T) -> (World, T) {
        {
            let mut slot = self.slot.lock().expect("the world lease is poisoned");
            debug_assert!(slot.is_none(), "the world was lent twice without a return");
            *slot = Some(world);
        }

        let produced = body();

        let world = self
            .slot
            .lock()
            .expect("the world lease is poisoned")
            .take()
            .expect("the world was lent and did not come back");
        (world, produced)
    }
}

/// The mod-facing end of a [`Lease`].
///
/// One of these is installed on the VM at startup and lives for the whole run;
/// what changes is whether there is a world in the slot behind it.
pub struct Shared {
    slot: Arc<Mutex<Option<World>>>,
}

impl Access for Shared {
    fn line_of_sight(&self, from: [f64; 3], to: [f64; 3]) -> Sighting {
        // A poisoned lease means the simulation thread panicked while the world
        // was lent, which is not something a mod should be told about with an
        // error it would have to handle.
        let Ok(slot) = self.slot.lock() else {
            return Sighting::Unavailable;
        };
        let Some(world) = slot.as_ref() else {
            return Sighting::Unavailable;
        };

        if tiamot_core::sight::between(world, from, to) {
            Sighting::Clear
        } else {
            Sighting::Blocked
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiamot_core::chunk::Chunk;
    use tiamot_core::coords::ChunkPos;

    fn world() -> World {
        let mut registry = tiamot_core::Registry::new();
        registry.register("test:stone").expect("register");
        let db = tiamot_core::persist::WorldDb::open_in_memory(&mut registry).expect("open");
        World::open(db, 1).expect("world")
    }

    /// A generator that makes nothing, so a chunk is loaded only where a test
    /// asked for one and everywhere else stays honestly absent.
    struct Empty;

    impl crate::world::ChunkSource for Empty {
        fn generate(&mut self, pos: ChunkPos, _world_seed: u64) -> Chunk {
            Chunk::air(pos)
        }
    }

    #[test]
    fn nothing_is_visible_outside_a_lend() {
        let lease = Lease::new();
        let handle = lease.handle();
        assert_eq!(
            handle.line_of_sight([0.0; 3], [1.0, 0.0, 0.0]),
            Sighting::Unavailable
        );
    }

    #[test]
    fn a_lent_world_answers_and_comes_back() {
        let lease = Lease::new();
        let handle = lease.handle();

        let mut world = world();
        world
            .chunk(ChunkPos::new(0, 0, 0), &mut Empty)
            .expect("the chunk loads");

        let (world, seen) = lease.lending(world, || {
            handle.line_of_sight([0.5, 0.5, 0.5], [2.5, 0.5, 0.5])
        });

        assert_eq!(seen, Sighting::Clear);
        assert_eq!(world.cached(), 1, "the world came back with its chunk");

        // And the slot is empty again, so the next mod call outside a lend is
        // told so rather than reading a world the tick has moved on from.
        assert_eq!(
            handle.line_of_sight([0.0; 3], [1.0, 0.0, 0.0]),
            Sighting::Unavailable
        );
    }

    #[test]
    fn terrain_the_world_has_not_loaded_blocks() {
        // The difference this test exists for: `Blocked` is an answer about the
        // world and `Unavailable` is an answer about the engine, and a mod that
        // cannot tell them apart cannot tell "the mimic lost sight of you"
        // from "the mimic was asked at the wrong moment".
        let lease = Lease::new();
        let handle = lease.handle();

        let (_world, seen) = lease.lending(world(), || {
            handle.line_of_sight([0.5, 0.5, 0.5], [2.5, 0.5, 0.5])
        });
        assert_eq!(seen, Sighting::Blocked);
    }
}
