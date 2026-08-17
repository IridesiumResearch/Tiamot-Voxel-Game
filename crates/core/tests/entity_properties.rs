// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Properties of the entity store that have to hold for *any* history.
//!
//! The unit tests in `core::ent` check the cases somebody thought of. These
//! check the invariants that no sequence of spawns and despawns may break,
//! which is where charter rule 4 actually lives: iteration order is a property
//! of the data, so two worlds holding the same live set must iterate
//! identically however they got there.

use proptest::prelude::*;
use tiamot_core::ChunkPos;
use tiamot_core::ent::{Entities, Entity, EntityId, Transform};

/// One thing a world can be asked to do.
#[derive(Debug, Clone)]
enum Step {
    /// Spawn an entity carrying a label, so a later assertion can name it.
    Spawn(u16),
    /// Despawn whichever live entity is at this position in iteration order.
    ///
    /// By POSITION rather than by id, because a generated id would almost
    /// always be stale and the interesting histories are the ones that really
    /// remove something.
    DespawnNth(u8),
    /// Despawn by a raw id, which is usually stale. This is the half that
    /// exercises the generation check.
    DespawnRaw(u8, u8),
}

fn steps() -> impl Strategy<Value = Vec<Step>> {
    prop::collection::vec(
        prop_oneof![
            3 => any::<u16>().prop_map(Step::Spawn),
            2 => any::<u8>().prop_map(Step::DespawnNth),
            1 => (any::<u8>(), any::<u8>()).prop_map(|(i, g)| Step::DespawnRaw(i, g)),
        ],
        0..64,
    )
}

fn mob(label: u16) -> Entity {
    Entity::at(
        Transform::at(ChunkPos::new(0, 0, 0), [0.0, 0.0, 0.0]),
        format!("test:{label}"),
    )
}

/// Runs a history and returns the world it produced.
fn run(script: &[Step]) -> Entities {
    let mut world = Entities::new();
    for step in script {
        match step {
            Step::Spawn(label) => {
                world.spawn(mob(*label));
            }
            Step::DespawnNth(nth) => {
                let ids = world.ids();
                if !ids.is_empty() {
                    world.despawn(ids[*nth as usize % ids.len()]);
                }
            }
            Step::DespawnRaw(index, generation) => {
                world.despawn(EntityId::new(u32::from(*index), u32::from(*generation)));
            }
        }
    }
    world
}

/// The labels a world hands out, in iteration order.
fn order(world: &Entities) -> Vec<String> {
    world
        .iter()
        .map(|(_, entity)| entity.source.clone())
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2_048))]

    /// **The same history gives the same world, twice.**
    ///
    /// The weak form of the determinism claim, and the one that catches a store
    /// whose internals are seeded per instance — Rust's `RandomState` advances
    /// per `HashMap`, so two worlds in ONE process would already differ.
    #[test]
    fn one_history_gives_one_world(script in steps()) {
        let first = run(&script);
        let second = run(&script);
        prop_assert_eq!(order(&first), order(&second));
        prop_assert_eq!(first.len(), second.len());
    }

    /// **Every live id resolves, and resolves to itself.**
    ///
    /// The property that makes a handle safe to hold: whatever the history,
    /// iteration never hands out an id that `get` then refuses, and never hands
    /// out two ids for one entity.
    #[test]
    fn every_id_iteration_hands_out_resolves(script in steps()) {
        let world = run(&script);
        let ids = world.ids();
        for id in &ids {
            prop_assert!(world.contains(*id), "iteration produced an id {id} that does not resolve");
        }
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        prop_assert_eq!(sorted.len(), ids.len(), "an entity was iterated twice");
        prop_assert_eq!(ids.len(), world.len(), "len disagrees with what iteration finds");
    }

    /// **A despawned id never resolves again.**
    ///
    /// Including after the slot is reused, which is the case generations exist
    /// for and the one a plain index would get wrong.
    #[test]
    fn a_despawned_id_stays_dead(script in steps(), extra in 0usize..8) {
        let mut world = run(&script);
        let Some(victim) = world.ids().first().copied() else {
            return Ok(());
        };
        prop_assert!(world.despawn(victim).is_some());
        // Refill, so the slot is very likely reused.
        for label in 0..extra {
            world.spawn(mob(label as u16));
        }
        prop_assert!(!world.contains(victim), "a despawned id came back to life");
        prop_assert!(world.despawn(victim).is_none(), "a stale id despawned its successor");
    }

    /// **The store round-trips byte for byte** (charter rule 8).
    ///
    /// Re-encoding the decoded value must give the identical bytes, not merely
    /// an equal value — the stronger claim, and the one that catches a decode
    /// that silently normalises something.
    #[test]
    fn any_world_round_trips_through_postcard(script in steps()) {
        let world = run(&script);
        let bytes = postcard::to_allocvec(&world).expect("encode");
        let back: Entities = postcard::from_bytes(&bytes).expect("decode");
        prop_assert_eq!(&back, &world);
        prop_assert_eq!(postcard::to_allocvec(&back).expect("re-encode"), bytes);
    }

    /// **Freezing a chunk and thawing it preserves order and contents.**
    ///
    /// A chunk unloading takes its entities out of the live store and its
    /// loading puts them back. What must survive is the entities and the order
    /// they were in — NOT their ids, which are handles into an arena the world
    /// no longer has.
    #[test]
    fn a_freeze_and_thaw_preserves_the_entities_and_their_order(script in steps()) {
        let mut world = run(&script);
        let before = order(&world);

        let frozen = world.take_chunk(ChunkPos::new(0, 0, 0));
        prop_assert!(world.is_empty(), "every entity in this history is in one chunk");
        prop_assert_eq!(
            frozen.iter().map(|e| e.source.clone()).collect::<Vec<_>>(),
            before.clone()
        );

        world.spawn_all(frozen);
        prop_assert_eq!(order(&world), before);
    }
}
