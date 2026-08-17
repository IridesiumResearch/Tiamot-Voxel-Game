// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Entities: what the world contains besides voxels.
//!
//! # A generational arena, and why not an ECS
//!
//! [`docs/ecs-verdict.md`](../../../docs/ecs-verdict.md) records the decision
//! and the measurements behind it. The short version: `bevy_ecs` and `hecs` are
//! both deterministic and both fast enough — stepping the task's gate of 200
//! entities costs under a microsecond in all three candidates, which is
//! 0.0008% of a tick at worst — so neither of the two axes anyone reaches for
//! decided anything. What decided it is that **this engine does not have the
//! problem an ECS solves**: the component set is fixed at compile time, and the
//! one place a general store would help is mods attaching their own state,
//! which it does not, because a mod attaches a Lua table the engine holds as one
//! opaque handle.
//!
//! So: a `Vec` of records indexed by a generational id.
//!
//! # Iteration order is index order, and that is load-bearing
//!
//! Charter rule 4 bans float accumulation over non-deterministic iteration
//! order. Here the order is `0..records.len()`, which is a property of the data
//! and not of a hash seed, a process, or a platform. Two servers given the same
//! sequence of spawns and despawns iterate identically because there is nowhere
//! for them to differ.
//!
//! The free list is **sorted**, so slot reuse is lowest-first rather than
//! whatever order entities happened to die in. Without that, two worlds could
//! reach the same set of live entities by different routes and lay them out
//! differently — same contents, different iteration order, divergent float sums.
//!
//! # Generations are what make a stale id safe
//!
//! Despawning bumps the slot's generation, so an id held across a despawn stops
//! resolving instead of resolving to whatever moved in. A mod holding an id for
//! a mob that died gets `None`, which is the only answer that cannot corrupt
//! anything. The generation saturates rather than wrapping — see [`Slot`].

pub mod component;

use std::collections::BTreeMap;

pub use component::{AnimTag, Collider, Health, ModelId, Nametag, Owner, Transform, Velocity};

use crate::coords::ChunkPos;

/// A handle to an entity.
///
/// # Layout
///
/// The slot index in the low 32 bits and the generation in the high 32. Packed
/// into one `u64` because it crosses two boundaries where a pair is awkward and
/// a scalar is not: the wire, and Lua. Lua 5.4 has a real 64-bit integer
/// subtype, so a mod holding one of these holds it exactly rather than through
/// an `f64` that would start rounding at 2^53.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct EntityId(pub u64);

impl EntityId {
    /// Builds an id from its two halves.
    #[must_use]
    pub const fn new(index: u32, generation: u32) -> Self {
        Self((generation as u64) << 32 | index as u64)
    }

    /// Which slot this id refers to.
    #[must_use]
    pub const fn index(self) -> u32 {
        self.0 as u32
    }

    /// Which occupant of that slot.
    #[must_use]
    pub const fn generation(self) -> u32 {
        (self.0 >> 32) as u32
    }
}

impl std::fmt::Display for EntityId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}#{}", self.index(), self.generation())
    }
}

/// Everything the engine knows about one entity.
///
/// The engine-defined components are fields. `Option` marks the ones an entity
/// may genuinely lack — a marker with no model, a rock with no hit points —
/// rather than the ones that happen to be unset.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Entity {
    /// Where it is. Every entity is somewhere.
    pub transform: Transform,
    /// How fast it is moving. Zero is a meaningful answer, so not optional.
    pub velocity: Velocity,
    /// The box it occupies, or `None` for something that does not collide.
    pub collider: Option<Collider>,
    /// What to draw, or `None` for something invisible.
    pub model: Option<ModelId>,
    /// What it is doing, for the client to pick a clip from.
    pub anim: AnimTag,
    /// Hit points, or `None` for something that cannot be hurt.
    pub health: Option<Health>,
    /// The label above it, or `None` for no label.
    pub nametag: Option<Nametag>,
    /// Who it belongs to, or `None` for nobody.
    pub owner: Option<Owner>,
    /// Which mod spawned it, as a string id.
    ///
    /// **Not decoration.** A mod that is removed from a world leaves its
    /// entities behind, and something has to be able to say which those are —
    /// exactly as `engine:unknown` preserves an unregistered block rather than
    /// deleting it (charter rule 8).
    pub source: String,
    /// The mod's own state, opaque to the engine.
    ///
    /// A serialised Lua table rather than a live handle, because this is what
    /// persists with the entity and a VM handle does not survive a restart. The
    /// script layer inflates it on demand and writes it back when it changes.
    pub script: Option<Vec<u8>>,
}

impl Entity {
    /// A bare entity at a transform, with nothing else set.
    #[must_use]
    pub fn at(transform: Transform, source: impl Into<String>) -> Self {
        Self {
            transform,
            velocity: Velocity::default(),
            collider: None,
            model: None,
            anim: AnimTag::default(),
            health: None,
            nametag: None,
            owner: None,
            source: source.into(),
            script: None,
        }
    }

    /// The chunk this entity is anchored to, which is where it persists.
    #[must_use]
    pub const fn chunk(&self) -> ChunkPos {
        self.transform.chunk
    }
}

/// One slot in the arena.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct Slot {
    /// Which occupant this is. Bumped on every despawn.
    ///
    /// **Saturating rather than wrapping.** A wrap would make a very old id
    /// start resolving again, to a different entity, silently — the one failure
    /// mode generations exist to prevent. At `u32::MAX` the slot stops being
    /// reused instead, which costs one slot in a world that has despawned four
    /// billion entities through it and cannot resurrect a stale handle.
    generation: u32,
    /// The entity, or `None` for a free slot.
    entity: Option<Entity>,
}

/// The world's entities.
///
/// Iteration is index order (see the module docs). Nothing here touches the
/// voxel world, the network, or the script VM: this is the data model, and the
/// systems that step and replicate it live above it.
#[derive(Debug, Default, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Entities {
    slots: Vec<Slot>,
    /// Free slots, **sorted**, lowest reused first. See the module docs.
    free: Vec<u32>,
    /// How many slots are occupied, so `len` is not a scan.
    live: usize,
}

impl Entities {
    /// An empty world.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many entities exist.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.live
    }

    /// Whether there are none.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// Adds an entity and returns its id.
    ///
    /// Reuses the lowest free slot, or appends. Which one it picks is part of
    /// the determinism contract, not an implementation detail: it decides the
    /// iteration order every later tick sees.
    pub fn spawn(&mut self, entity: Entity) -> EntityId {
        self.live += 1;
        if self.free.is_empty() {
            let index = u32::try_from(self.slots.len()).unwrap_or(u32::MAX);
            self.slots.push(Slot {
                generation: 0,
                entity: Some(entity),
            });
            return EntityId::new(index, 0);
        }
        let index = self.free.remove(0);
        let slot = &mut self.slots[index as usize];
        slot.entity = Some(entity);
        EntityId::new(index, slot.generation)
    }

    /// Removes an entity, returning it if the id was live.
    ///
    /// A stale id removes nothing and returns `None`, which is what makes it
    /// safe for a mod to hold one indefinitely.
    pub fn despawn(&mut self, id: EntityId) -> Option<Entity> {
        let slot = self.slots.get_mut(id.index() as usize)?;
        if slot.generation != id.generation() {
            return None;
        }
        let entity = slot.entity.take()?;
        self.live -= 1;
        // Saturating: see `Slot::generation`. A saturated slot is never freed,
        // so no id can ever resolve to it again.
        if slot.generation == u32::MAX {
            return Some(entity);
        }
        slot.generation += 1;
        let index = id.index();
        // Sorted insert rather than push-then-sort: the list is already sorted,
        // so this is a memmove and the sort would be a comparison per element
        // on every despawn.
        let at = self.free.partition_point(|free| *free < index);
        self.free.insert(at, index);
        Some(entity)
    }

    /// Borrows an entity, or `None` if the id is stale or was never live.
    #[must_use]
    pub fn get(&self, id: EntityId) -> Option<&Entity> {
        let slot = self.slots.get(id.index() as usize)?;
        (slot.generation == id.generation()).then_some(slot.entity.as_ref()?)
    }

    /// Borrows an entity mutably.
    pub fn get_mut(&mut self, id: EntityId) -> Option<&mut Entity> {
        let slot = self.slots.get_mut(id.index() as usize)?;
        if slot.generation != id.generation() {
            return None;
        }
        slot.entity.as_mut()
    }

    /// Whether an id resolves.
    #[must_use]
    pub fn contains(&self, id: EntityId) -> bool {
        self.get(id).is_some()
    }

    /// Every live entity, in slot order.
    pub fn iter(&self) -> impl Iterator<Item = (EntityId, &Entity)> {
        self.slots.iter().enumerate().filter_map(|(index, slot)| {
            let entity = slot.entity.as_ref()?;
            Some((EntityId::new(index as u32, slot.generation), entity))
        })
    }

    /// Every live entity, mutably, in slot order.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (EntityId, &mut Entity)> {
        self.slots
            .iter_mut()
            .enumerate()
            .filter_map(|(index, slot)| {
                let generation = slot.generation;
                let entity = slot.entity.as_mut()?;
                Some((EntityId::new(index as u32, generation), entity))
            })
    }

    /// Every live id, in slot order.
    ///
    /// Collected rather than borrowed, for the case that needs it: stepping
    /// entities that may spawn or despawn others. Iterating while the store
    /// changes underneath is the classic way to get a different answer on two
    /// machines.
    #[must_use]
    pub fn ids(&self) -> Vec<EntityId> {
        self.iter().map(|(id, _)| id).collect()
    }

    /// Every entity within `radius` cells of `centre`, nearest first.
    ///
    /// # A linear scan, deliberately, for now
    ///
    /// There is no spatial index. At the task's gate of 200 entities a scan is
    /// a few microseconds, and at 2,000 it is tens — against a 50 ms tick
    /// (charter rule 18). An index would have to be kept correct through every
    /// move, which is a cost on the path that runs every tick to save one that
    /// runs when a mod asks. Revisit when a measurement says so, not before.
    ///
    /// Ties are broken by id, so the order is total and two servers agree.
    #[must_use]
    pub fn within(&self, centre: &Transform, radius: f32) -> Vec<(EntityId, f32)> {
        let bound = radius * radius;
        let mut found: Vec<(EntityId, f32)> = self
            .iter()
            .filter_map(|(id, entity)| {
                let distance = centre.distance_squared(&entity.transform);
                (distance <= bound).then_some((id, distance))
            })
            .collect();
        // `total_cmp` rather than `partial_cmp`: a NaN would make the ordering
        // inconsistent and `sort_by` may then panic. Simulation must not
        // produce NaN at all (charter rule 4), so this is belt and braces
        // against a mod that set a transform by hand.
        found.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
        found
    }

    /// Live entities grouped by the chunk they are anchored to.
    ///
    /// A `BTreeMap` rather than a `HashMap`, and the ids within each chunk stay
    /// in slot order — this feeds persistence, and charter rule 8 requires a
    /// world to round-trip byte for byte.
    #[must_use]
    pub fn by_chunk(&self) -> BTreeMap<ChunkPos, Vec<EntityId>> {
        let mut grouped: BTreeMap<ChunkPos, Vec<EntityId>> = BTreeMap::new();
        for (id, entity) in self.iter() {
            grouped.entry(entity.chunk()).or_default().push(id);
        }
        grouped
    }

    /// Removes every entity anchored to a chunk and returns them in slot order.
    ///
    /// This is what a chunk unloading does: entities do not go on being
    /// simulated somewhere nobody can see, they go into the chunk's blob and
    /// come back when it loads. See [`Self::spawn_all`] for the other half.
    pub fn take_chunk(&mut self, chunk: ChunkPos) -> Vec<Entity> {
        let doomed: Vec<EntityId> = self
            .iter()
            .filter_map(|(id, entity)| (entity.chunk() == chunk).then_some(id))
            .collect();
        doomed
            .into_iter()
            .filter_map(|id| self.despawn(id))
            .collect()
    }

    /// Spawns a batch in order, returning their new ids.
    ///
    /// The ids are NOT the ones the entities had before they were frozen, and
    /// nothing may assume they are: an id is a handle into this arena and a
    /// world reloads into a different arena. Anything that must survive a
    /// freeze belongs in the entity, not in its id.
    pub fn spawn_all(&mut self, entities: impl IntoIterator<Item = Entity>) -> Vec<EntityId> {
        entities
            .into_iter()
            .map(|entity| self.spawn(entity))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn somewhere(x: f32) -> Transform {
        Transform::at(ChunkPos::new(0, 0, 0), [x, 0.0, 0.0])
    }

    fn mob(x: f32) -> Entity {
        // The source doubles as a label, so the order assertions can compare
        // strings rather than floats — `clippy::float_cmp` is deny-level in
        // this crate and the tests take no exemption from it.
        Entity::at(somewhere(x), format!("test:mob{x}"))
    }

    #[test]
    fn an_id_carries_its_slot_and_its_generation() {
        let id = EntityId::new(7, 3);
        assert_eq!(id.index(), 7);
        assert_eq!(id.generation(), 3);
        assert_eq!(id.to_string(), "7#3");
    }

    #[test]
    fn spawn_get_and_despawn_round_trip() {
        let mut world = Entities::new();
        assert!(world.is_empty());

        let id = world.spawn(mob(1.0));
        assert_eq!(world.len(), 1);
        assert_eq!(world.get(id).map(|e| e.source.as_str()), Some("test:mob1"));

        world.get_mut(id).expect("live").velocity = Velocity([1.0, 0.0, 0.0]);
        assert_eq!(
            world.get(id).expect("live").velocity,
            Velocity([1.0, 0.0, 0.0])
        );

        let taken = world.despawn(id).expect("was live");
        assert_eq!(taken.source, "test:mob1");
        assert_eq!(world.len(), 0);
        assert!(world.get(id).is_none());
    }

    #[test]
    fn an_id_held_across_a_despawn_does_not_resolve_to_the_next_occupant() {
        // **The whole reason for generations.** A mod holding the id of a mob
        // that died must get `None`, not whatever moved into its slot — which
        // is the same slot, because the free list hands the lowest one back.
        let mut world = Entities::new();
        let first = world.spawn(mob(1.0));
        world.despawn(first);
        let second = world.spawn(mob(2.0));

        assert_eq!(first.index(), second.index(), "the slot really was reused");
        assert!(
            world.get(first).is_none(),
            "a stale id resolved to the entity that replaced it"
        );
        assert_eq!(world.get(second).expect("live").source, "test:mob2");
        assert!(
            world.despawn(first).is_none(),
            "a stale id despawned the entity that replaced it"
        );
        assert_eq!(
            world.len(),
            1,
            "and took it out of the world while doing so"
        );
    }

    #[test]
    fn iteration_order_is_slot_order_whatever_route_the_world_took_to_get_there() {
        // Charter rule 4: the order simulation visits entities in must be a
        // property of the data, not of the history that produced it. Two worlds
        // with the same live set, reached differently, must iterate the same.
        let mut direct = Entities::new();
        for x in 0..6 {
            direct.spawn(mob(x as f32));
        }
        direct.despawn(EntityId::new(2, 0));
        direct.despawn(EntityId::new(4, 0));

        let mut churned = Entities::new();
        for x in 0..6 {
            churned.spawn(mob(x as f32));
        }
        // The same two removed, in the other order.
        churned.despawn(EntityId::new(4, 0));
        churned.despawn(EntityId::new(2, 0));

        let order = |world: &Entities| -> Vec<String> {
            world.iter().map(|(_, e)| e.source.clone()).collect()
        };
        assert_eq!(order(&direct), order(&churned));

        // And the sorted free list means the next spawn lands in the same slot
        // in both, which is what keeps them agreeing from here on.
        let a = direct.spawn(mob(99.0));
        let b = churned.spawn(mob(99.0));
        assert_eq!(a, b, "the two worlds disagree about which slot to reuse");
        assert_eq!(order(&direct), order(&churned));
    }

    #[test]
    fn within_finds_the_near_ones_nearest_first() {
        let mut world = Entities::new();
        let near = world.spawn(mob(1.0));
        let far = world.spawn(mob(40.0));
        let middle = world.spawn(mob(5.0));

        let found = world.within(&somewhere(0.0), 10.0);
        assert_eq!(
            found.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![near, middle],
            "the far one is outside the radius and the near ones are ordered"
        );
        assert!(!found.iter().any(|(id, _)| *id == far));
    }

    #[test]
    fn within_reaches_across_a_chunk_boundary() {
        // The frame bug again, from the query's side: an entity one cell over a
        // chunk edge is one cell away, not a chunk away and not zero.
        let mut world = Entities::new();
        let span = crate::CHUNK_SUBNODES as f32;
        let over = world.spawn(Entity::at(
            Transform::at(ChunkPos::new(1, 0, 0), [0.0, 0.0, 0.0]),
            "test:mob",
        ));
        let here = Transform::at(ChunkPos::new(0, 0, 0), [span - 1.0, 0.0, 0.0]);

        let found = world.within(&here, 2.0);
        assert_eq!(found.len(), 1, "the entity across the seam was not found");
        assert_eq!(found[0].0, over);
        assert!(
            (found[0].1 - 1.0).abs() < f32::EPSILON,
            "it should be one cell away, squared; got {}",
            found[0].1
        );
    }

    #[test]
    fn a_chunk_unloading_takes_its_entities_and_loading_brings_them_back() {
        let mut world = Entities::new();
        let home = ChunkPos::new(3, 0, 0);
        let away = ChunkPos::new(4, 0, 0);
        world.spawn(Entity::at(Transform::at(home, [1.0; 3]), "test:a"));
        world.spawn(Entity::at(Transform::at(away, [2.0; 3]), "test:b"));
        world.spawn(Entity::at(Transform::at(home, [3.0; 3]), "test:c"));

        let frozen = world.take_chunk(home);
        assert_eq!(frozen.len(), 2);
        assert_eq!(
            frozen.iter().map(|e| e.source.as_str()).collect::<Vec<_>>(),
            vec!["test:a", "test:c"],
            "frozen in slot order, which is what makes the blob reproducible"
        );
        assert_eq!(world.len(), 1, "the other chunk's entity stayed");

        let thawed = world.spawn_all(frozen);
        assert_eq!(thawed.len(), 2);
        assert_eq!(world.len(), 3);
        assert_eq!(
            world.by_chunk().get(&home).map(Vec::len),
            Some(2),
            "and they came back to the chunk they left from"
        );
    }

    #[test]
    fn the_store_round_trips_through_postcard() {
        // Charter rule 8: a world's data round-trips byte for byte. The store
        // is serialised whole here rather than per chunk, which is the strongest
        // form of the claim.
        let mut world = Entities::new();
        let id = world.spawn(Entity {
            health: Some(Health::full(20)),
            nametag: Some(Nametag::Text("a name".into())),
            model: Some(ModelId::HUMANOID),
            collider: Some(Collider::HUMANOID),
            anim: AnimTag::WALK,
            script: Some(vec![1, 2, 3]),
            ..mob(1.0)
        });
        world.spawn(mob(2.0));
        world.despawn(id);

        let bytes = postcard::to_allocvec(&world).expect("encode");
        let back: Entities = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(back, world);
        assert_eq!(
            postcard::to_allocvec(&back).expect("re-encode"),
            bytes,
            "re-encoding produced different bytes"
        );
    }

    #[test]
    fn a_saturated_generation_retires_the_slot_rather_than_wrapping() {
        // A wrap would make a very old id start resolving again, to a different
        // entity, silently — the one thing generations exist to prevent.
        let mut world = Entities::new();
        let id = world.spawn(mob(1.0));
        world.slots[id.index() as usize].generation = u32::MAX;
        let stale = EntityId::new(id.index(), u32::MAX);

        assert!(world.despawn(stale).is_some());
        assert!(
            world.free.is_empty(),
            "the retired slot went back on the free list"
        );

        let next = world.spawn(mob(2.0));
        assert_ne!(next.index(), stale.index(), "the retired slot was reused");
    }
}
