// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The server's entities: stepping them, freezing them, and thawing them.
//!
//! [`tiamot_core::ent`] is the data model and knows nothing about the world,
//! the clock, or the database. This is where it meets all three.
//!
//! # Entities move through the player's physics, not beside it
//!
//! Charter rule 2 allows exactly one simulation. A mob falls, collides with
//! sub-node geometry, steps up a lip and floats in milk because it runs
//! [`tiamot_core::phys::step`] — the same function, with the same
//! [`Tuning`](tiamot_core::phys::Tuning), that the player has been running since
//! Task 09. The only thing an entity brings of its own is the size of its box,
//! which is why [`tiamot_core::phys::Aabb::sized_at`] exists.
//!
//! What a mob *decides* is not the engine's business (charter rule 1). The
//! engine reads [`Entity::drive`](tiamot_core::ent::Entity::drive) — a walk
//! direction, a jump, a gait — and a mod writes it. That is the same shape as a
//! player's input queue, deliberately: the engine moves bodies, and something
//! else says where they are trying to go.
//!
//! # Freezing is removal, not a flag
//!
//! An entity in an unloaded chunk is not stepped, not replicated, and not in
//! memory. Its chunk's rows in the world database are the entity. That is
//! stronger than a "frozen" flag, which would leave a growing population of
//! things nobody can see being skipped by every loop for the rest of the
//! session.

use std::collections::{BTreeMap, BTreeSet};

use tiamot_core::coords::ChunkPos;
use tiamot_core::ent::{Entities, Entity, EntityId, Transform, Velocity};
use tiamot_core::phys::{self, Body};

use crate::world::World;

/// The `source` every player mirror carries.
///
/// The engine's own namespace, not a mod's — the same one charter rule 8 keeps
/// `engine:unknown` in. A mod cannot register under it, so `owned_by_mod` never
/// hands a player's body to anybody's step callback, and a mod filtering
/// `entities_in_radius` by source can ask for exactly the players.
pub const PLAYER_SOURCE: &str = "engine:player";

/// Every live entity, and the bookkeeping the tick needs around them.
#[derive(Debug, Default)]
pub struct Population {
    entities: Entities,
    /// Chunks whose entities have changed since the last save.
    ///
    /// A `BTreeSet` for the reason `Fluidics::dirty` is one: the order rows are
    /// written in is not a simulation result, but reaching for a `HashSet`
    /// inside the tick is the habit that eventually puts one there.
    dirty: BTreeSet<ChunkPos>,
    /// Chunks whose entities have been read from the database this session.
    ///
    /// Separate from what `entities` holds, because a chunk that was read and
    /// found empty belongs here and not there — without the distinction an
    /// empty chunk is re-read from the database every time it arrives.
    loaded: BTreeSet<ChunkPos>,
    /// Entities that mirror something the engine already owns.
    ///
    /// A player's body lives in `transport::Shared::bodies` and is moved by
    /// their inputs. It is *also* an entity, because everything that asks
    /// "what is near me" — a mod, a client's renderer, the replication
    /// tracker — should get one answer with one shape (charter rule 2). This
    /// set is what keeps the mirror from being mistaken for a real one:
    ///
    /// - **Never persisted.** A world file that saved players would grow a
    ///   corpse for everyone who ever visited, standing where they logged out.
    /// - **Never dirties a chunk.** A mirror is written every tick, and a
    ///   player walking would otherwise mark every chunk they cross for saving,
    ///   twenty times a second.
    /// - **Never stepped.** Its physics has already happened; stepping it again
    ///   would apply gravity to a body that has already fallen this tick.
    transient: BTreeSet<EntityId>,
    /// Which entity mirrors which player.
    players: BTreeMap<tiamot_core::PlayerUuid, EntityId>,
}

impl Population {
    /// An empty world.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The entities themselves.
    #[must_use]
    pub const fn entities(&self) -> &Entities {
        &self.entities
    }

    /// How many entities are live.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.entities.len()
    }

    /// Whether none are.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    /// The entity mirroring a player, if they have one.
    ///
    /// The replication tracker's `exclude`: nobody needs to be told where they
    /// are by the machine they are telling, and a client drawing its own body
    /// through its own eyes sees the inside of its own head.
    #[must_use]
    pub fn player_entity(&self, uuid: &tiamot_core::PlayerUuid) -> Option<EntityId> {
        self.players.get(uuid).copied()
    }

    /// Creates or updates the entity mirroring one player.
    ///
    /// Called every tick for every connected player, after their physics has
    /// run — which is why none of this marks anything dirty. See
    /// [`Population::transient`].
    pub fn sync_player(
        &mut self,
        uuid: tiamot_core::PlayerUuid,
        transform: Transform,
        velocity: Velocity,
        on_ground: bool,
        anim: tiamot_core::ent::AnimTag,
        hands: tiamot_core::ent::Hands,
    ) -> EntityId {
        if let Some(&id) = self.players.get(&uuid)
            && let Some(entity) = self.entities.get_mut(id)
        {
            entity.transform = transform;
            entity.velocity = velocity;
            entity.on_ground = on_ground;
            entity.anim = anim;
            entity.hands = hands;
            return id;
        }

        // `self.entities.spawn`, not `self.spawn`: the latter marks the chunk
        // for saving, which is the one thing a mirror must never do.
        let mut entity = Entity::at(transform, PLAYER_SOURCE);
        entity.velocity = velocity;
        entity.on_ground = on_ground;
        entity.anim = anim;
        entity.hands = hands;
        entity.model = Some(tiamot_core::ent::HUMANOID_MODEL.to_owned());
        // The player's own box, so a client culls the drawn body against the
        // same shape the server collided it with (charter rule 2).
        entity.collider = Some(phys::Shape::HUMANOID);
        entity.owner = Some(tiamot_core::ent::Owner(uuid));
        // The UUID and not the name (charter rule 13): the current display name
        // is resolved when the spawn is sent, so a rebinding follows.
        entity.nametag = Some(tiamot_core::ent::Nametag::Player(uuid));
        let id = self.entities.spawn(entity);
        self.transient.insert(id);
        self.players.insert(uuid, id);
        id
    }

    /// The entity mirroring a connected player, or `None` if they are not here.
    ///
    /// The index the mirror already keeps, not a scan: a body goes in the
    /// moment a player connects and comes out when they go.
    #[must_use]
    pub fn player_body(&self, uuid: &tiamot_core::PlayerUuid) -> Option<EntityId> {
        self.players.get(uuid).copied()
    }

    /// Removes the mirrors of players who are no longer connected.
    ///
    /// Takes the set that IS here rather than the one that left, because a
    /// disconnect the tick never saw would otherwise leave a body standing in
    /// the world for ever — and the roster is the thing the server is sure of.
    pub fn retain_players(&mut self, present: &BTreeSet<tiamot_core::PlayerUuid>) {
        let gone: Vec<tiamot_core::PlayerUuid> = self
            .players
            .keys()
            .filter(|uuid| !present.contains(*uuid))
            .copied()
            .collect();
        for uuid in gone {
            if let Some(id) = self.players.remove(&uuid) {
                self.transient.remove(&id);
                // The arena directly, again: a mirror leaving must not mark a
                // chunk for saving either.
                self.entities.despawn(id);
            }
        }
    }

    /// Whether this chunk's entities have been read this session.
    #[must_use]
    pub fn knows(&self, pos: ChunkPos) -> bool {
        self.loaded.contains(&pos)
    }

    /// Adds an entity and marks its chunk for saving.
    pub fn spawn(&mut self, entity: Entity) -> EntityId {
        self.dirty.insert(entity.chunk());
        self.entities.spawn(entity)
    }

    /// Removes an entity, marking its chunk for saving.
    pub fn despawn(&mut self, id: EntityId) -> Option<Entity> {
        let entity = self.entities.despawn(id)?;
        self.dirty.insert(entity.chunk());
        Some(entity)
    }

    /// Borrows an entity.
    #[must_use]
    pub fn get(&self, id: EntityId) -> Option<&Entity> {
        self.entities.get(id)
    }

    /// Borrows an entity for writing, marking its chunk for saving.
    ///
    /// **Pessimistic on purpose.** The caller may only be reading a field, but
    /// the alternative is trusting every call site to say when it wrote — and
    /// the failure mode of getting that wrong is a mob that silently stops
    /// persisting. A spurious chunk save costs one row rewrite.
    pub fn get_mut(&mut self, id: EntityId) -> Option<&mut Entity> {
        let chunk = self.entities.get(id)?.chunk();
        self.dirty.insert(chunk);
        self.entities.get_mut(id)
    }

    /// A chunk's entities have arrived from the database.
    ///
    /// Idempotent: a chunk that arrives twice does not get its population
    /// doubled. The lighting defers what it cannot relight by putting a chunk
    /// back into `take_arrived`, so **chunks genuinely do arrive twice** — the
    /// same trap `Fluidics::loaded` exists for.
    pub fn chunk_loaded(&mut self, pos: ChunkPos, entities: Vec<Entity>) {
        if !self.loaded.insert(pos) {
            return;
        }
        self.entities.spawn_all(entities);
    }

    /// A chunk is going away: take its entities so they can be written.
    ///
    /// The chunk stops being "known", so it will be read again if it comes
    /// back. Returns them in slot order, which is the order they must be
    /// written in — see [`tiamot_core::persist::WorldDb::load_chunk_entities`].
    pub fn freeze(&mut self, pos: ChunkPos) -> Vec<Entity> {
        // A chunk somebody is standing in is not a chunk to unload, and the
        // mirror in it is not something to write to disk. Refusing is the whole
        // guard: `take_chunk` removes everything anchored to the chunk, so
        // without this a player's body would be frozen out from under them and
        // saved into the world file as a corpse.
        if self.players.values().any(|id| {
            self.entities
                .get(*id)
                .is_some_and(|held| held.chunk() == pos)
        }) {
            return Vec::new();
        }
        self.loaded.remove(&pos);
        let frozen = self.entities.take_chunk(pos);
        if !frozen.is_empty() {
            // Marked dirty even though the entities are leaving: what is on
            // disk has to end up matching what was in memory, and the caller
            // may be freezing a chunk whose contents changed since it loaded.
            self.dirty.insert(pos);
        }
        frozen
    }

    /// Marks a chunk as needing a save.
    pub fn mark_dirty(&mut self, pos: ChunkPos) {
        self.dirty.insert(pos);
    }

    /// How many chunks are waiting to be written.
    #[must_use]
    pub fn dirty(&self) -> usize {
        self.dirty.len()
    }

    /// Takes the chunks needing a write, and what to write into each.
    ///
    /// A chunk whose last entity left yields an empty `Vec`, so the caller can
    /// hand the whole sequence to the database and let the deletes and the
    /// writes land together. Without that, the last mob to leave a chunk would
    /// come straight back the next time it loaded.
    pub fn take_dirty(&mut self) -> Vec<(ChunkPos, Vec<Entity>)> {
        let dirty = std::mem::take(&mut self.dirty);
        if dirty.is_empty() {
            return Vec::new();
        }
        let mut grouped = self.entities.by_chunk();
        dirty
            .into_iter()
            .map(|pos| {
                let ids = grouped.remove(&pos).unwrap_or_default();
                let entities = ids
                    .into_iter()
                    .filter(|id| !self.transient.contains(id))
                    .filter_map(|id| self.entities.get(id).cloned())
                    .collect();
                (pos, entities)
            })
            .collect()
    }

    /// Live entity ids grouped by the mod that spawned them.
    ///
    /// For the per-entity step callbacks: the engine does the grouping because
    /// it is the only side that knows whose entity is whose. A `BTreeMap` and
    /// slot order within each mod, so the call order is a property of the data.
    #[must_use]
    pub fn owned_by_mod(&self) -> BTreeMap<String, Vec<u64>> {
        let mut grouped: BTreeMap<String, Vec<u64>> = BTreeMap::new();
        for (id, entity) in self.entities.iter() {
            grouped.entry(entity.source.clone()).or_default().push(id.0);
        }
        grouped
    }

    /// Every entity that takes up room, for the crowd-separation pass.
    ///
    /// **Player mirrors are left out.** A player is in this store as a
    /// transient copy of a body the tick steps itself, so including one would
    /// have every player shove themselves — and the shove would land on the
    /// mirror, which is overwritten from the real body a moment later.
    ///
    /// Slot order, which is the order charter rule 4 requires be a property of
    /// the data rather than of a hash.
    pub fn crowd(&self) -> impl Iterator<Item = (EntityId, tiamot_core::phys::Occupant)> + '_ {
        self.entities.ids().into_iter().filter_map(move |id| {
            if self.transient.contains(&id) {
                return None;
            }
            let entity = self.entities.get(id)?;
            let collider = entity.collider?;
            Some((
                id,
                tiamot_core::phys::Occupant {
                    origin: entity.transform.chunk,
                    position: entity.transform.local,
                    height: collider.height,
                    width: collider.width,
                    // Everything with a box is pushable for now. See
                    // `Occupant::movable` for what the other answer is for.
                    movable: true,
                },
            ))
        })
    }

    /// Adds a horizontal nudge to an entity's velocity.
    ///
    /// Applied before [`Self::tick`] steps it, so terrain still decides where
    /// it ends up. Silently does nothing for an id that has gone, which is
    /// ordinary: the crowd was read a few statements ago.
    pub fn nudge(&mut self, id: EntityId, push: [f32; 3]) {
        if let Some(entity) = self.entities.get_mut(id) {
            entity.velocity.0[0] += push[0];
            entity.velocity.0[2] += push[2];
        }
    }

    /// Steps every entity by one tick.
    ///
    /// # Order
    ///
    /// Slot order, over a snapshot of the ids taken before anything moves. Both
    /// halves matter: slot order is what charter rule 4 requires be a property
    /// of the data, and the snapshot is what stops an entity spawned during the
    /// step from being stepped in the same tick it was created — which would
    /// make a mob's first tick depend on which slot it happened to land in.
    ///
    /// # Only entities with a collider move
    ///
    /// A marker with no box has no physics to run. It can still be moved by a
    /// mod writing its transform; it simply does not fall.
    pub fn tick(&mut self, domain: &str, world: &World, fluid: &crate::fluid::Fluidics) {
        // Bound once for the whole pass: every body here is in this domain, and
        // `ChunkLookup` has no way to carry one.
        let terrain = world.solid(domain);
        for id in self.entities.ids() {
            // A player's mirror has already moved this tick, under its own
            // inputs. Stepping it again would apply a second tick of gravity to
            // a body that has had one — and the correction would arrive as the
            // other players on your screen sinking into the floor.
            if self.transient.contains(&id) {
                continue;
            }
            let Some(entity) = self.entities.get(id) else {
                continue;
            };
            let Some(collider) = entity.collider else {
                continue;
            };

            let origin = entity.transform.chunk;
            let body = Body {
                position: entity.transform.local,
                velocity: entity.velocity.0,
                on_ground: entity.on_ground,
            };
            let drive = entity.drive;

            // Resident chunks only, exactly as the player's step does: a mob
            // walking into unloaded terrain stops rather than generating a
            // chunk inside the tick budget. It reads the fluid too, so a mob
            // floats in the same milk a player does — charter rule 2, one
            // simulation.
            let voxels = phys::Voxels::with_fluid(&terrain, fluid, origin);
            let stepped = phys::step_shaped(&voxels, body, drive, &phys::Tuning::DEFAULT, collider);

            // Charter rule 7: keep the local part inside one chunk so it never
            // becomes a world-space f32 that loses precision far from the
            // origin. The chunk an entity is anchored to is also where it
            // persists, so this is what moves a mob between chunk rows.
            let (moved, local) = phys::voxels::renormalise(origin, stepped.position);
            let (chunk, local) = if moved.in_world() {
                (moved, local)
            } else {
                // The world is finite (charter rule 6). A mob over the edge
                // stops rather than falling for ever — the same answer the
                // player gets, and for the same reason: an entity falling
                // without end drags its interest set down with it.
                (origin, stepped.position)
            };

            let Some(entity) = self.entities.get_mut(id) else {
                continue;
            };
            if chunk != entity.transform.chunk {
                // It changed chunks, so BOTH are dirty: the one it left has to
                // stop claiming it, and the one it arrived in has to start.
                // Marking only the destination is how a world fills up with
                // copies of everything that ever moved.
                self.dirty.insert(entity.transform.chunk);
                self.dirty.insert(chunk);
            }
            entity.transform.chunk = chunk;
            entity.transform.local = local;
            entity.velocity.0 = if moved.in_world() {
                stepped.velocity
            } else {
                [0.0; 3]
            };
            entity.on_ground = stepped.on_ground;
        }
    }
}

/// A handle on the entity store, for the mod API.
///
/// The same arrangement `fluid::Shared` uses, behind a lock for the same
/// reason: `game.spawn_entity` runs inside a tick, on the simulation thread,
/// and cannot borrow what the tick is holding. Uncontended in practice — both
/// sides are that one thread — and never held across a mod callback, which is
/// the arrangement that would deadlock.
pub struct Shared {
    population: std::sync::Arc<std::sync::RwLock<Population>>,
    /// The players' authoritative bodies.
    ///
    /// **Not the mirrors in `population`.** A mod moving a player has to write
    /// the body the tick steps, because the mirror is a copy of it that is
    /// overwritten every tick — see `Access::move_player`.
    bodies: std::sync::Arc<crate::transport::PlayerBodies>,
}

impl Shared {
    /// Wraps the stores the simulation thread owns.
    #[must_use]
    pub const fn new(
        population: std::sync::Arc<std::sync::RwLock<Population>>,
        bodies: std::sync::Arc<crate::transport::PlayerBodies>,
    ) -> Self {
        Self { population, bodies }
    }
}

impl tiamot_core::ent::Access for Shared {
    fn spawn(&self, entity: Entity) -> Option<EntityId> {
        // A poisoned lock means the simulation thread panicked, in which case
        // there is no world to spawn into. `None` is the honest answer, and
        // panicking inside a mod callback would blame the mod for it.
        self.population
            .write()
            .ok()
            .map(|mut population| population.spawn(entity))
    }

    fn despawn(&self, id: EntityId) -> bool {
        self.population
            .write()
            .is_ok_and(|mut population| population.despawn(id).is_some())
    }

    fn get(&self, id: EntityId) -> Option<Entity> {
        self.population
            .read()
            .ok()
            .and_then(|population| population.get(id).cloned())
    }

    fn patch(&self, id: EntityId, patch: &tiamot_core::ent::Patch) -> bool {
        let Ok(mut population) = self.population.write() else {
            return false;
        };
        population
            .get_mut(id)
            .is_some_and(|entity| patch.apply(entity))
    }

    fn player(&self, uuid: [u8; 32]) -> Option<EntityId> {
        let uuid = tiamot_core::PlayerUuid::from_bytes(uuid);
        // The mirror index the tick already keeps, not a scan: a body is put
        // there the moment a player connects and taken out when they go.
        self.population.read().ok()?.player_body(&uuid)
    }

    fn move_player(&self, uuid: [u8; 32], to: [f64; 3]) -> bool {
        let uuid = tiamot_core::PlayerUuid::from_bytes(uuid);
        // A world position, split back into the (chunk, local) pair charter
        // rule 7 requires — the same conversion an entity's `pos` patch makes,
        // and for the same reason: a world-space `f32` loses precision long
        // before the world runs out.
        let at = tiamot_core::ent::Transform::from_world(to[0], to[1], to[2]);
        if !at.chunk.in_world() {
            return false;
        }
        let Ok(mut bodies) = self.bodies.lock() else {
            return false;
        };
        let Some(player) = bodies.get_mut(&uuid) else {
            return false;
        };
        player.origin = at.chunk;
        player.body.position = at.local;
        // **Velocity goes with them.** A player teleported mid-fall who kept
        // their speed arrives at the far end already moving, which reads as
        // the destination throwing them.
        player.body.velocity = [0.0; 3];
        true
    }

    fn shove_player(&self, uuid: [u8; 32], impulse: [f32; 3]) -> bool {
        let uuid = tiamot_core::PlayerUuid::from_bytes(uuid);
        let Ok(mut bodies) = self.bodies.lock() else {
            return false;
        };
        let Some(player) = bodies.get_mut(&uuid) else {
            return false;
        };
        for (velocity, push) in player.body.velocity.iter_mut().zip(impulse) {
            *velocity += push;
        }
        // Off the ground, or the next step's friction eats a shove that was
        // meant to move somebody standing still.
        if impulse[1] > 0.0 {
            player.body.on_ground = false;
        }
        true
    }

    fn within(&self, centre: [f64; 3], radius: f64, source: Option<&str>) -> Vec<EntityId> {
        let Ok(population) = self.population.read() else {
            return Vec::new();
        };
        let centre = tiamot_core::ent::Transform::from_world(centre[0], centre[1], centre[2]);
        // Blocks in, cells inside: a mod says "within 32 yards" and the engine
        // knows that is 96 cells (charter rule 5).
        let cells = radius * f64::from(tiamot_core::SUBNODES_PER_AXIS);
        population
            .entities()
            .within(&centre, cells as f32)
            .into_iter()
            .filter(|(id, _)| match source {
                None => true,
                Some(wanted) => population
                    .get(*id)
                    .is_some_and(|entity| entity.source == wanted),
            })
            .map(|(id, _)| id)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_crowd_leaves_out_the_mirrors_of_players_who_move_themselves() {
        // A player is in this store as a transient copy of a body the tick
        // steps itself. Including one would have every player shove
        // themselves, and the shove would land on the mirror — which is
        // overwritten from the real body a moment later, so the bug would be
        // invisible everywhere except in how a player felt.
        let mut population = Population::new();
        let real = population.spawn(mob(ChunkPos::new(0, 0, 0), [8.0, 0.0, 8.0]));
        population.sync_player(
            tiamot_core::PlayerUuid::from_bytes([7; 32]),
            Transform::at(ChunkPos::new(0, 0, 0), [8.5, 0.0, 8.0]),
            tiamot_core::ent::Velocity([0.0; 3]),
            true,
            tiamot_core::ent::AnimTag::IDLE,
            tiamot_core::ent::Hands::default(),
        );

        let crowd: Vec<EntityId> = population.crowd().map(|(id, _)| id).collect();
        assert_eq!(
            crowd,
            vec![real],
            "a player's mirror was counted as somebody to push"
        );
    }

    #[test]
    fn a_body_in_the_crowd_is_pushed_out_of_a_players_space() {
        // The entity half of "players should very subtly collide with mobs".
        // A mob standing where a player is standing takes a nudge, and the
        // nudge is horizontal — a crowd that can push up lifts things through
        // ceilings.
        let mut population = Population::new();
        let standing = mob(ChunkPos::new(0, 0, 0), [8.0, 0.0, 8.0]);
        let id = population.spawn(standing);

        let player = tiamot_core::phys::Occupant {
            origin: ChunkPos::new(0, 0, 0),
            position: [8.2, 0.0, 8.0],
            height: tiamot_core::phys::PLAYER_HEIGHT,
            width: tiamot_core::phys::PLAYER_WIDTH,
            movable: true,
        };
        let mut occupants = vec![player];
        occupants.extend(population.crowd().map(|(_, occupant)| occupant));
        let pushes = tiamot_core::phys::separate(&occupants);
        population.nudge(id, pushes[1]);

        let velocity = population.get(id).expect("the mob").velocity.0;
        assert!(
            velocity[0] < 0.0,
            "a mob sharing a player's space was not pushed out of it: {velocity:?}"
        );
        assert_eq!(
            velocity[1].to_bits(),
            0,
            "the mob was pushed vertically, which is how a crowd lifts things"
        );
    }

    use tiamot_core::MaterialId;
    use tiamot_core::ent::{Collider, Transform};

    const STONE: MaterialId = MaterialId(2);

    fn mob(chunk: ChunkPos, local: [f32; 3]) -> Entity {
        Entity {
            collider: Some(Collider::HUMANOID),
            ..Entity::at(Transform::at(chunk, local), "test:mob")
        }
    }

    /// A generator that makes nothing, so a test decides the contents.
    struct Empty;

    impl crate::world::ChunkSource for Empty {
        fn generate(&mut self, pos: ChunkPos, _seed: u64) -> tiamot_core::chunk::Chunk {
            tiamot_core::chunk::Chunk::air(pos)
        }
    }

    fn world() -> World {
        let mut registry = tiamot_core::Registry::new();
        registry.register("test:stone").expect("register");
        let db = tiamot_core::persist::WorldDb::open_in_memory(&mut registry).expect("open");
        World::open(db, 1).expect("world")
    }

    /// A chunk with a solid floor in its bottom block layer.
    fn floor(world: &mut World, pos: ChunkPos) {
        world
            .chunk(tiamot_core::domain::OVERWORLD, pos, &mut Empty)
            .expect("chunk");
        let corner = tiamot_core::BlockPos::from_chunk_corner(pos);
        fill(world, corner.y, STONE);
    }

    /// Fills one block layer of the chunk at the origin.
    fn fill(world: &mut World, y: i32, material: MaterialId) {
        for x in 0..16 {
            for z in 0..16 {
                world
                    .apply(
                        tiamot_core::domain::OVERWORLD,
                        &tiamot_core::proto::Edit::Block {
                            pos: tiamot_core::BlockPos::new(x, y, z),
                            material: material.get(),
                        },
                        &mut Empty,
                    )
                    .expect("place");
            }
        }
    }

    fn player() -> tiamot_core::PlayerUuid {
        tiamot_core::identity::Identity::generate()
            .expect("identity")
            .uuid_as_root()
    }

    fn somewhere(chunk: ChunkPos) -> Transform {
        Transform::at(chunk, [24.0, 4.0, 24.0])
    }

    #[test]
    fn a_players_mirror_is_never_written_to_the_world() {
        // The failure this exists for: a world file that saved players grows a
        // corpse for everyone who ever visited, standing where they logged out,
        // and every one of them comes back as a real entity on the next load.
        let mut population = Population::new();
        let home = ChunkPos::new(1, 0, 1);
        let uuid = player();

        population.sync_player(
            uuid,
            somewhere(home),
            Velocity::default(),
            true,
            tiamot_core::ent::AnimTag::IDLE,
            tiamot_core::ent::Hands::default(),
        );
        // A real mob in the same chunk, so the chunk genuinely needs saving and
        // the test is about what goes IN the row rather than whether one exists.
        population.spawn(Entity::at(somewhere(home), "test:mob"));

        let saved = population.take_dirty();
        let entities: Vec<&Entity> = saved
            .iter()
            .filter(|(pos, _)| *pos == home)
            .flat_map(|(_, held)| held.iter())
            .collect();
        assert_eq!(entities.len(), 1, "the mirror was written to the world");
        assert_eq!(entities[0].source, "test:mob");
    }

    #[test]
    fn a_walking_player_does_not_dirty_every_chunk_they_cross() {
        // A mirror is written every tick. If that marked its chunk for saving,
        // one player walking would be a database write per chunk per tick, for
        // as long as they kept moving.
        let mut population = Population::new();
        let uuid = player();
        population.sync_player(
            uuid,
            somewhere(ChunkPos::new(0, 0, 0)),
            Velocity::default(),
            true,
            tiamot_core::ent::AnimTag::IDLE,
            tiamot_core::ent::Hands::default(),
        );
        assert!(
            population.take_dirty().is_empty(),
            "spawning a mirror dirtied a chunk"
        );

        for step in 1..8 {
            population.sync_player(
                uuid,
                somewhere(ChunkPos::new(step, 0, 0)),
                Velocity::default(),
                true,
                tiamot_core::ent::AnimTag::WALK,
                tiamot_core::ent::Hands::default(),
            );
        }
        assert!(
            population.take_dirty().is_empty(),
            "walking across chunks marked them for saving"
        );
    }

    #[test]
    fn a_mirror_is_updated_in_place_rather_than_respawned() {
        // The id has to be stable: a client is told about entities by id, and a
        // mirror that despawned and respawned every tick would be a spawn and a
        // despawn message per player per tick, and a body that flickered.
        let mut population = Population::new();
        let uuid = player();
        let first = population.sync_player(
            uuid,
            somewhere(ChunkPos::new(0, 0, 0)),
            Velocity::default(),
            true,
            tiamot_core::ent::AnimTag::IDLE,
            tiamot_core::ent::Hands::default(),
        );
        let again = population.sync_player(
            uuid,
            somewhere(ChunkPos::new(3, 0, 0)),
            Velocity([1.0, 0.0, 0.0]),
            false,
            tiamot_core::ent::AnimTag::WALK,
            tiamot_core::ent::Hands::default(),
        );
        assert_eq!(first, again);
        assert_eq!(population.len(), 1);

        let entity = population.get(first).expect("the mirror is there");
        assert_eq!(entity.transform.chunk, ChunkPos::new(3, 0, 0));
        assert_eq!(entity.anim, tiamot_core::ent::AnimTag::WALK);
        assert!(!entity.on_ground);
        assert_eq!(entity.owner.map(|owner| owner.0), Some(uuid));
        assert_eq!(entity.source, PLAYER_SOURCE);
    }

    #[test]
    fn a_player_who_leaves_takes_their_body_with_them() {
        let mut population = Population::new();
        let stays = player();
        let goes = player();
        for uuid in [stays, goes] {
            population.sync_player(
                uuid,
                somewhere(ChunkPos::new(0, 0, 0)),
                Velocity::default(),
                true,
                tiamot_core::ent::AnimTag::IDLE,
                tiamot_core::ent::Hands::default(),
            );
        }
        assert_eq!(population.len(), 2);

        population.retain_players(&BTreeSet::from([stays]));
        assert_eq!(population.len(), 1);
        assert!(population.player_entity(&goes).is_none());
        assert!(population.player_entity(&stays).is_some());
        assert!(
            population.take_dirty().is_empty(),
            "a player logging out marked a chunk for saving"
        );
    }

    #[test]
    fn a_mirror_is_not_stepped_by_the_entity_physics() {
        // Its physics already ran, from the player's own inputs. A second step
        // would be a second tick of gravity, and the correction arrives as the
        // other players on your screen sinking into the floor.
        let mut world = world();
        let home = ChunkPos::new(0, 0, 0);
        world
            .chunk(tiamot_core::domain::OVERWORLD, home, &mut Empty)
            .expect("chunk");

        let mut population = Population::new();
        let uuid = player();
        let id = population.sync_player(
            uuid,
            somewhere(home),
            Velocity::default(),
            true,
            tiamot_core::ent::AnimTag::IDLE,
            tiamot_core::ent::Hands::default(),
        );
        let before = population.get(id).expect("mirror").transform;

        let fluid = crate::fluid::Fluidics::default();
        for _ in 0..10 {
            population.tick(tiamot_core::domain::OVERWORLD, &world, &fluid);
        }

        let after = population.get(id).expect("mirror").transform;
        assert_eq!(
            before.local, after.local,
            "the mirror fell; the player's own physics is the only thing that may move it"
        );
    }

    #[test]
    fn a_chunk_somebody_is_standing_in_is_not_frozen() {
        let mut population = Population::new();
        let home = ChunkPos::new(2, 0, 2);
        population.sync_player(
            player(),
            somewhere(home),
            Velocity::default(),
            true,
            tiamot_core::ent::AnimTag::IDLE,
            tiamot_core::ent::Hands::default(),
        );
        population.spawn(Entity::at(somewhere(home), "test:mob"));

        assert!(
            population.freeze(home).is_empty(),
            "a chunk with a player in it was unloaded, taking their body with it"
        );
        assert_eq!(population.len(), 2, "freezing removed entities anyway");
    }

    #[test]
    fn spawning_and_despawning_marks_the_chunk_for_saving() {
        let mut population = Population::new();
        let home = ChunkPos::new(1, 2, 3);
        let id = population.spawn(mob(home, [1.0; 3]));
        assert_eq!(population.dirty(), 1);

        let written = population.take_dirty();
        assert_eq!(written.len(), 1);
        assert_eq!(written[0].0, home);
        assert_eq!(written[0].1.len(), 1);
        assert_eq!(population.dirty(), 0);

        population.despawn(id);
        let written = population.take_dirty();
        assert_eq!(
            written,
            vec![(home, Vec::new())],
            "the chunk a mob left must be written EMPTY, or the mob comes back \
             the next time the chunk loads"
        );
    }

    #[test]
    fn a_chunk_arriving_twice_does_not_double_its_population() {
        // Chunks genuinely do arrive twice: the lighting defers what it cannot
        // relight by putting the chunk back into `take_arrived`. The same trap
        // `Fluidics::loaded` exists for.
        let mut population = Population::new();
        let home = ChunkPos::new(0, 0, 0);
        population.chunk_loaded(home, vec![mob(home, [1.0; 3])]);
        population.chunk_loaded(home, vec![mob(home, [1.0; 3])]);
        assert_eq!(population.len(), 1);
        assert!(population.knows(home));
    }

    #[test]
    fn freezing_a_chunk_takes_its_entities_and_forgets_it() {
        let mut population = Population::new();
        let home = ChunkPos::new(5, 0, 0);
        let away = ChunkPos::new(6, 0, 0);
        population.chunk_loaded(home, vec![mob(home, [1.0; 3]), mob(home, [2.0; 3])]);
        population.chunk_loaded(away, vec![mob(away, [3.0; 3])]);

        let frozen = population.freeze(home);
        assert_eq!(frozen.len(), 2);
        assert_eq!(population.len(), 1, "the other chunk's mob stayed");
        assert!(
            !population.knows(home),
            "a frozen chunk must be re-read when it comes back, or its entities \
             are gone for the rest of the session"
        );
        assert!(population.knows(away));
    }

    #[test]
    fn a_mob_falls_and_lands_through_the_players_own_physics() {
        // **Charter rule 2 in one test.** A mob is not simulated beside the
        // player, it is simulated by the same function — so it falls under the
        // same gravity, stops a skin short of the surface the same way, and
        // ends up with `on_ground` set by the same ground probe.
        let mut world = world();
        let home = ChunkPos::new(0, 0, 0);
        floor(&mut world, home);

        let mut population = Population::new();
        // Cells: the floor's top surface is at y = 3, one block up.
        let id = population.spawn(mob(home, [24.0, 30.0, 24.0]));
        let fluid = crate::fluid::Fluidics::default();

        for _ in 0..200 {
            population.tick(tiamot_core::domain::OVERWORLD, &world, &fluid);
        }

        let entity = population.get(id).expect("live");
        assert!(entity.on_ground, "the mob never landed");
        let feet = entity.transform.local[1];
        assert!(
            (feet - 3.0).abs() < 0.05,
            "the mob came to rest at {feet} rather than on the floor at 3.0"
        );
    }

    #[test]
    fn a_short_mob_walks_under_a_lintel_a_humanoid_cannot() {
        // **The proof that the collider is used rather than carried.** Two
        // mobs, identical but for their height, walking at a wall whose bottom
        // block layer is missing: the short one goes through the gap and the
        // humanoid stops at the face. If the physics ignored the shape they
        // would end up in the same place.
        let mut world = world();
        let home = ChunkPos::new(0, 0, 0);
        floor(&mut world, home);
        // A wall four blocks east, from block y=2 up — so the gap under it is
        // cells 3..6, three cells: room for a two-cell mob and not for a
        // humanoid's 5.4.
        for y in 2..5 {
            for z in 0..16 {
                world
                    .apply(
                        tiamot_core::domain::OVERWORLD,
                        &tiamot_core::proto::Edit::Block {
                            pos: tiamot_core::BlockPos::new(4, y, z),
                            material: STONE.get(),
                        },
                        &mut Empty,
                    )
                    .expect("place wall");
            }
        }

        let east = tiamot_core::phys::Intent {
            fly: false,
            walk: [1.0, 0.0],
            jump: false,
            gait: tiamot_core::phys::Gait::Walk,
        };
        let squat = Collider {
            width: 1.0,
            height: 2.0,
        };
        let mut population = Population::new();
        let start = 6.0;
        let small = population.spawn(Entity {
            collider: Some(squat),
            drive: east,
            ..mob(home, [start, 3.0, 24.0])
        });
        let tall = population.spawn(Entity {
            drive: east,
            ..mob(home, [start, 3.0, 30.0])
        });

        let fluid = crate::fluid::Fluidics::default();
        for _ in 0..120 {
            population.tick(tiamot_core::domain::OVERWORLD, &world, &fluid);
        }

        let travelled = |id| population.get(id).expect("live").transform.local[0] - start;
        // The wall's west face is at cell 12, and a humanoid's half-width is
        // 0.9, so it comes to rest with its centre just short of 11.1.
        assert!(
            travelled(tall) < 5.2,
            "the humanoid travelled {} and so went through a wall its head does \
             not fit under",
            travelled(tall)
        );
        assert!(
            travelled(small) > 7.0,
            "the short mob travelled only {} and so did not fit through a gap \
             twice its height",
            travelled(small)
        );
    }

    #[test]
    fn an_entity_with_no_collider_does_not_fall() {
        // A marker is a position and nothing else. It has no box, so there is
        // no physics to run on it — and a mod moving it by writing its
        // transform is the whole point of it existing.
        let mut population = Population::new();
        let home = ChunkPos::new(0, 4, 0);
        let marker = population.spawn(Entity::at(Transform::at(home, [24.0; 3]), "test:marker"));

        let world = world();
        let fluid = crate::fluid::Fluidics::default();
        for _ in 0..10 {
            population.tick(tiamot_core::domain::OVERWORLD, &world, &fluid);
        }

        let entity = population.get(marker).expect("live");
        assert!(
            (entity.transform.local[1] - 24.0).abs() < f32::EPSILON,
            "a marker with no collider fell to {}",
            entity.transform.local[1]
        );
    }
}
