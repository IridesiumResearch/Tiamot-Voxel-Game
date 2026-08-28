// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The seam a mod reaches entities through.
//!
//! The same arrangement [`crate::fluid::Access`] uses, and for the same reason:
//! the store lives in the server and the script VM lives in `core`, which
//! cannot depend on it (charter rule 3). `&self` rather than `&mut self`,
//! because the VM hands one handle to every mod environment and cannot lend it
//! out mutably; every caller is the simulation thread inside a tick, so the lock
//! behind it is uncontended and is never held across a mod callback.
//!
//! # A patch, not a whole entity
//!
//! A mod does not write an [`Entity`] back. It says which fields it wants
//! changed, and everything it did not mention keeps whatever the tick just gave
//! it. Read-modify-write would let a mod's copy — taken at the top of `on_step`,
//! written at the bottom — silently undo the position the physics computed in
//! between, and it would let a mod overwrite `source`, which is the engine's
//! record of who to blame for the entity.

use super::{AnimTag, Entity, EntityId, Transform};

/// What a mod may change about an entity.
///
/// Every field is `None` for "leave it alone". Nothing here can create or
/// destroy an entity, change its size, or change which mod owns it: those are
/// spawn-time decisions, and a mod that wants a different one despawns and
/// spawns again.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Patch {
    /// Where to put it, in world blocks.
    ///
    /// **Teleports.** It does not sweep, so a mod may put an entity inside a
    /// wall — where the physics will leave it, because a body that begins a tick
    /// inside geometry stays there (Sub-Node Contract §2). Moving something by
    /// steering it is what [`Self::drive`] is for.
    pub position: Option<[f64; 3]>,
    /// Velocity, in cells per tick.
    pub velocity: Option<[f32; 3]>,
    /// Facing, in radians about the vertical axis.
    pub yaw: Option<f32>,
    /// How far up or down it looks, in radians.
    pub pitch: Option<f32>,
    /// What it is trying to do, which the next tick's physics reads.
    pub drive: Option<crate::phys::Intent>,
    /// Which clip a client should play.
    pub anim: Option<AnimTag>,
    /// Current hit points. Ignored by an entity that has no health at all.
    pub health: Option<u32>,
    /// The mod's own state, opaque to the engine.
    pub script: Option<Vec<u8>>,
}

impl Patch {
    /// Applies this to an entity, returning whether anything changed.
    ///
    /// Charter rule 4: clamped rather than clamping later, because a `NaN` or an
    /// infinity written by a mod would reach the physics and then the wire, and
    /// simulation state must not contain one. A mod that writes nonsense gets
    /// its write dropped rather than a diverged world.
    pub fn apply(&self, entity: &mut Entity) -> bool {
        let mut changed = false;
        if let Some(position) = self.position.filter(|p| p.iter().all(|v| v.is_finite())) {
            entity.transform = Transform {
                yaw: entity.transform.yaw,
                pitch: entity.transform.pitch,
                ..Transform::from_world(position[0], position[1], position[2])
            };
            changed = true;
        }
        if let Some(velocity) = self.velocity.filter(|v| v.iter().all(|v| v.is_finite())) {
            entity.velocity.0 = velocity;
            changed = true;
        }
        if let Some(yaw) = self.yaw.filter(|v| v.is_finite()) {
            entity.transform.yaw = yaw;
            changed = true;
        }
        if let Some(pitch) = self.pitch.filter(|v| v.is_finite()) {
            entity.transform.pitch = pitch;
            changed = true;
        }
        if let Some(drive) = self.drive.filter(|d| d.walk.iter().all(|v| v.is_finite())) {
            entity.drive = drive;
            changed = true;
        }
        if let Some(anim) = self.anim {
            entity.anim = anim;
            changed = true;
        }
        if let Some(health) = self.health
            && let Some(existing) = entity.health.as_mut()
        {
            existing.current = health.min(existing.max);
            changed = true;
        }
        if let Some(script) = self.script.clone() {
            entity.script = Some(script);
            changed = true;
        }
        changed
    }
}

/// Where a mod's entity calls reach.
pub trait Access: Send + Sync {
    /// Adds an entity and returns its id.
    fn spawn(&self, entity: Entity) -> Option<EntityId>;

    /// Removes an entity. Returns whether it was there to remove.
    fn despawn(&self, id: EntityId) -> bool;

    /// A copy of an entity, or `None` if the id is stale.
    ///
    /// A copy rather than a borrow: the store is behind a lock, and handing a
    /// reference out would mean holding that lock across whatever the mod does
    /// next — including calls back into the engine. Entities are small and a
    /// mod reads a handful per tick.
    fn get(&self, id: EntityId) -> Option<Entity>;

    /// Changes an entity. Returns whether anything changed.
    fn patch(&self, id: EntityId, patch: &Patch) -> bool;

    /// The entity mirroring a connected player, by their UUID.
    ///
    /// **Charter rule 13's other half.** Every hook hands a mod a UUID — who
    /// dug, who spoke, who pressed a key — and every entity call takes an id,
    /// so without this a mod that knows WHO cannot find out WHERE. Scanning the
    /// world for a body whose owner matches is the alternative, and it is a
    /// scan of everything to answer a question the server already has a map
    /// for.
    ///
    /// `None` for a player who is not connected, and for a UUID that has never
    /// been seen. Ids are per-session and a player's changes every time they
    /// join, so this is a lookup rather than something to remember.
    fn player(&self, uuid: [u8; 32]) -> Option<EntityId>;

    /// Every entity within `radius` blocks of a world position, nearest first.
    ///
    /// `source` filters by which mod spawned it, since that is the only label
    /// the engine has an opinion about. A mod wanting a finer filter puts it in
    /// its own script state and checks it itself — the engine has no idea what a
    /// "hostile" is (charter rule 1).
    fn within(&self, centre: [f64; 3], radius: f64, source: Option<&str>) -> Vec<EntityId>;

    /// Puts a connected player's body somewhere else.
    ///
    /// **Why this is not [`Access::patch`] on their mirror.** A player is in
    /// the entity store as a transient COPY of a body the tick steps from
    /// their own inputs, and that copy is overwritten every tick — so writing
    /// a position to it does nothing, silently, which is the worst shape a
    /// failure can take. This writes the authoritative body, and the ordinary
    /// correction the client already applies carries it.
    ///
    /// A teleport, not a walk: nothing is swept, so a mod that names the
    /// inside of a wall gets a player inside a wall. That is the same bargain
    /// [`Patch::pos`](super::Patch) makes for a mob and for the same reason —
    /// the alternative is the engine refusing a move a mod meant.
    ///
    /// Returns whether it happened. `false` means the player is not connected
    /// or the position is outside the world.
    fn move_player(&self, uuid: [u8; 32], to: [f64; 3]) -> bool;

    /// Adds to a connected player's velocity, in cells per tick.
    ///
    /// Knockback, an explosion, a jump pad. Added rather than set, because
    /// every one of those is a push ON a body that is already doing something
    /// — and a mod that wants to stop somebody dead can read the velocity off
    /// their mirror and cancel it.
    ///
    /// Returns whether the player was there to push.
    fn shove_player(&self, uuid: [u8; 32], impulse: [f32; 3]) -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coords::ChunkPos;
    use crate::ent::{Health, Transform};

    fn mob() -> Entity {
        Entity {
            health: Some(Health::full(20)),
            ..Entity::at(Transform::at(ChunkPos::new(0, 0, 0), [0.0; 3]), "test:mob")
        }
    }

    #[test]
    fn an_empty_patch_changes_nothing() {
        let mut entity = mob();
        let before = entity.clone();
        assert!(!Patch::default().apply(&mut entity));
        assert_eq!(entity, before);
    }

    #[test]
    fn a_patch_moves_an_entity_without_disturbing_where_it_is_looking() {
        // Position and facing are separate questions, and a mod setting one
        // must not silently reset the other — which is what building a fresh
        // `Transform` from world coordinates would do, since `from_world` has
        // no idea which way anything is facing.
        let mut entity = mob();
        entity.transform.yaw = 1.25;
        let moved = Patch {
            position: Some([100.5, 64.0, -3.0]),
            ..Patch::default()
        };
        assert!(moved.apply(&mut entity));

        let [x, y, z] = entity.transform.to_world();
        assert!((x - 100.5).abs() < 1.0 / 48.0, "x came out {x}");
        assert!((y - 64.0).abs() < 1.0 / 48.0, "y came out {y}");
        assert!((z + 3.0).abs() < 1.0 / 48.0, "z came out {z}");
        assert!(
            (entity.transform.yaw - 1.25).abs() < f32::EPSILON,
            "moving an entity reset its facing to {}",
            entity.transform.yaw
        );
    }

    #[test]
    fn a_patch_that_would_put_a_nan_in_simulation_state_is_dropped() {
        // Charter rule 4 bans producing NaN in simulation state, and a mod is
        // perfectly capable of computing one — `0/0` in Lua is a quiet NaN. It
        // would reach the physics and then the wire. Dropping the write is the
        // only answer that leaves the world consistent.
        let mut entity = mob();
        let poison = Patch {
            position: Some([f64::NAN, 0.0, 0.0]),
            velocity: Some([f32::INFINITY, 0.0, 0.0]),
            yaw: Some(f32::NAN),
            ..Patch::default()
        };
        assert!(!poison.apply(&mut entity), "a NaN write was accepted");
        assert!(entity.transform.local.iter().all(|v| v.is_finite()));
        assert!(entity.velocity.0.iter().all(|v| v.is_finite()));
        assert!(entity.transform.yaw.is_finite());
    }

    #[test]
    fn health_is_clamped_to_the_maximum_and_ignored_where_there_is_none() {
        let mut entity = mob();
        assert!(
            Patch {
                health: Some(1_000),
                ..Patch::default()
            }
            .apply(&mut entity)
        );
        assert_eq!(entity.health.expect("has health").current, 20);

        // A rock has no hit points, and giving it some through a patch would be
        // adding a component — which is a spawn-time decision.
        let mut rock = Entity::at(Transform::at(ChunkPos::new(0, 0, 0), [0.0; 3]), "test:rock");
        assert!(
            !Patch {
                health: Some(5),
                ..Patch::default()
            }
            .apply(&mut rock)
        );
        assert!(rock.health.is_none());
    }
}
