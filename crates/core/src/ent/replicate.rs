// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Deciding what each viewer is told about which entities.
//!
//! Pure logic over the store and one viewer's position — no socket, no world,
//! no clock. The transport asks what to send and sends it; everything that
//! decides *what* lives here, where it can be tested in microseconds instead of
//! through a network integration test. The same split
//! [`crate::interest`] makes for chunks, and for the same reason.
//!
//! # Interest is the chunk cylinder, reused
//!
//! An entity is interesting to a viewer exactly when the chunk it is anchored
//! to is one the viewer is being sent. That is not an approximation of a
//! distance test — it is the *right* rule, because an entity in a chunk the
//! viewer does not have is an entity standing on terrain that is not there.
//! Reusing [`crate::interest::contains`] also means the two can never disagree
//! about the edge of the world a player can see.
//!
//! # Three kinds of message, because they have three lifetimes
//!
//! - **Spawn** — everything about an entity, sent once, reliably. A client that
//!   missed this has an id it cannot draw.
//! - **Despawn** — sent once, reliably, for the same reason: a lost despawn is
//!   a mob that stands in the world for ever.
//! - **Delta** — position, velocity, look and animation, sent repeatedly and
//!   unreliably. A lost one is corrected by the next, 50 ms later.
//!
//! Losing the reliable pair is unrecoverable and losing a delta costs nothing,
//! which is exactly the division a reliable and an unreliable channel make.

use std::collections::BTreeMap;

use super::{AnimTag, Collider, Entities, Entity, EntityId, Nametag, Transform, Velocity};
use crate::coords::ChunkPos;
use crate::interest::{ViewDistance, contains};

/// How far an entity may move before a viewer is told, in cells.
///
/// A twelfth of a cell is about 28 mm, well under what a player can see at any
/// distance an entity is legible at — and far above the noise a body at rest
/// produces, which is exactly zero: `phys::step` leaves a resting body's
/// position bit-identical.
///
/// The threshold exists for the case that would otherwise dominate: a herd of
/// settled mobs standing still. Without it every one of them costs a delta
/// twenty times a second, for ever, to say nothing changed.
pub const MOVE_EPSILON: f32 = 1.0 / 12.0;

/// How far a look may turn before a viewer is told, in quantised steps.
///
/// One step, which is what quantisation makes the smallest possible change.
/// There is no cheaper answer than "it changed" once the value is a byte.
const LOOK_EPSILON: u8 = 1;

/// Everything a client needs to start drawing an entity.
#[derive(Debug, Clone, PartialEq)]
pub struct Spawn {
    /// Which entity.
    pub id: EntityId,
    /// Where it is.
    pub transform: Transform,
    /// How fast it is going, so a client can extrapolate from the first frame.
    pub velocity: Velocity,
    /// What to draw, or `None` for something invisible.
    pub model: Option<String>,
    /// How big it is, for the client's own culling.
    pub collider: Option<Collider>,
    /// The stack it looks like, for an item on the ground.
    ///
    /// **On the spawn and not in the delta.** What an item IS never changes
    /// while it lies there — a stack that changed would be a different item —
    /// so putting it in the twenty-times-a-second message would be paying for
    /// it on every one of them. See [`Entity::item`].
    pub item: Option<crate::inventory::Stack>,
    /// What it is holding.
    ///
    /// **On the spawn AND in its own message**, unlike [`Spawn::item`]: what an
    /// item IS never changes, and what a body is HOLDING changes every time
    /// somebody scrolls the wheel. It is not in [`Delta`] either — that is the
    /// unreliable twenty-a-second channel and a lost hand would stay lost.
    pub hands: crate::ent::Hands,
    /// What it is doing.
    pub anim: AnimTag,
    /// The label above it, unresolved.
    ///
    /// A [`Nametag::Player`] carries a UUID, and the *current* display name
    /// bound to it is a fact only the server's roster has (charter rule 13).
    /// **The caller resolves it to text before sending**, and this module does
    /// not, because `core` has no roster and inventing one so that a pure
    /// function could look a name up would be the wrong shape entirely.
    ///
    /// Resolved on the server rather than the client for the same rule: sending
    /// the UUID would make every client keep a roster it has no other use for,
    /// and a rebound name would stay stale on every screen until someone
    /// reconnected.
    pub nametag: Option<Nametag>,
}

/// What an entity a viewer already knows about is holding now.
///
/// Its own message rather than a field on [`Delta`], because the two have
/// opposite shapes: a position changes every tick and may be dropped, and a
/// hand changes rarely and may not. Putting it in the delta would pay for it
/// twenty times a second to carry something that is usually the same, on a
/// channel where losing it would leave a sword invisible until the next time
/// the player switched slots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Armed {
    /// Which entity.
    pub id: EntityId,
    /// What it is holding now.
    pub hands: crate::ent::Hands,
}

/// A change to an entity a viewer already knows about.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Delta {
    /// Which entity.
    pub id: EntityId,
    /// Where it is now.
    pub chunk: ChunkPos,
    /// Cell offset within that chunk.
    pub local: [f32; 3],
    /// Cells per tick, for the client to extrapolate between updates.
    pub velocity: [f32; 3],
    /// Facing, quantised to a byte: 256 steps around the circle.
    ///
    /// A byte is 1.4 degrees, which is well inside what a humanoid's shoulders
    /// can express and far inside what anyone can see on a mob across a field.
    /// Sending an `f32` would be four times the bytes for a precision the model
    /// cannot show.
    pub yaw: u8,
    /// Pitch, quantised the same way over the half circle.
    pub pitch: i8,
    /// What it is doing.
    pub anim: AnimTag,
}

/// Quantises a yaw in radians to a byte.
///
/// `rem_euclid` rather than a modulo, so a negative angle lands in `0..1` and
/// not in `-1..0` — the same trap `BlockPos::local` avoids with the same
/// function. Deterministic: division, multiplication and a truncating cast.
#[must_use]
pub fn quantise_yaw(radians: f32) -> u8 {
    let turns = radians / std::f32::consts::TAU;
    let wrapped = turns.rem_euclid(1.0);
    (wrapped * 256.0) as u8
}

/// Quantises a pitch in radians to a signed byte.
///
/// Clamped to a quarter turn each way, because a body that has bent further
/// back than straight up is not a thing the rig can express, and wrapping it
/// would make a mob look at the sky when it meant to look at the ground.
#[must_use]
pub fn quantise_pitch(radians: f32) -> i8 {
    let quarter = std::f32::consts::FRAC_PI_2;
    let clamped = radians.clamp(-quarter, quarter);
    (clamped / quarter * 127.0) as i8
}

/// What a viewer has been told, and what still needs telling.
///
/// One per connected client. Small: an id and a handful of numbers per entity
/// the viewer can see, and entities the viewer cannot see are not in it at all.
#[derive(Debug, Default, Clone)]
pub struct Tracker {
    known: BTreeMap<EntityId, Sent>,
}

/// The last thing a viewer was told about one entity.
#[derive(Debug, Clone, PartialEq)]
struct Sent {
    chunk: ChunkPos,
    local: [f32; 3],
    yaw: u8,
    pitch: i8,
    anim: AnimTag,
    hands: crate::ent::Hands,
}

impl Sent {
    fn of(entity: &Entity) -> Self {
        Self {
            chunk: entity.transform.chunk,
            local: entity.transform.local,
            yaw: quantise_yaw(entity.transform.yaw),
            pitch: quantise_pitch(entity.transform.pitch),
            anim: entity.anim,
            hands: entity.hands.clone().clone(),
        }
    }

    /// Whether the difference is worth a packet.
    fn differs_from(&self, other: &Self) -> bool {
        if self.chunk != other.chunk || self.anim != other.anim {
            return true;
        }
        if self.yaw.abs_diff(other.yaw) >= LOOK_EPSILON
            || self.pitch.abs_diff(other.pitch) >= LOOK_EPSILON
        {
            return true;
        }
        (0..3).any(|axis| (self.local[axis] - other.local[axis]).abs() >= MOVE_EPSILON)
    }
}

/// What to send one viewer this tick.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Update {
    /// Entities that came into view, or were created in it.
    pub spawned: Vec<Spawn>,
    /// Entities that left view, or were destroyed in it.
    pub despawned: Vec<EntityId>,
    /// Entities that moved enough to be worth a packet.
    pub moved: Vec<Delta>,
    /// Entities whose hands changed.
    ///
    /// Separate from `moved` because it goes on the reliable channel: a lost
    /// position is corrected 50 ms later and a lost hand is not corrected at
    /// all until the holder next changes it.
    pub rearmed: Vec<Armed>,
}

impl Update {
    /// Whether there is nothing to send.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.spawned.is_empty()
            && self.despawned.is_empty()
            && self.moved.is_empty()
            && self.rearmed.is_empty()
    }
}

impl Tracker {
    /// A viewer that has been told nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many entities this viewer is being kept up to date about.
    #[must_use]
    pub fn len(&self) -> usize {
        self.known.len()
    }

    /// Whether the viewer is tracking nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.known.is_empty()
    }

    /// Forgets everything, so the next update re-spawns what is in view.
    ///
    /// For a client that reconnected, or that asked for a different view
    /// distance and has thrown its own entity list away.
    pub fn clear(&mut self) {
        self.known.clear();
    }

    /// Works out what to send, and records that it was sent.
    ///
    /// `exclude` is the viewer's own entity where they have one — nobody needs
    /// to be told where they are by the machine they are telling.
    ///
    /// `shares_space` says whether an entity is in the same simulation space as
    /// the viewer. **A predicate rather than a domain id**, because [`Entities`]
    /// has no idea what a domain is: which space a body is in is the server's
    /// book (`Population`), and threading the whole book through here to answer
    /// one question per entity would put a server concept in a pure function.
    ///
    /// Without it a player in a ship is told about every mob standing at the
    /// same coordinates in the overworld — chunk interest alone cannot tell
    /// them apart, because the coordinates are identical.
    ///
    /// # The recording happens here on purpose
    ///
    /// Splitting "decide" from "record" would leave a window where a caller
    /// that dropped an update would go on believing it had been sent. The
    /// unreliable channel already loses deltas and that is fine, because the
    /// next one corrects it — but a lost SPAWN that the tracker recorded is an
    /// entity the client can never be told about again.
    ///
    /// So: the caller must send what this returns. The reliable half of the
    /// channel is what makes that a safe thing to require.
    pub fn update(
        &mut self,
        entities: &Entities,
        centre: ChunkPos,
        view: ViewDistance,
        exclude: Option<EntityId>,
        shares_space: &dyn Fn(EntityId) -> bool,
    ) -> Update {
        let mut update = Update::default();
        let mut still_visible = BTreeMap::new();

        // Slot order, so two servers given the same world send the same
        // messages in the same order — which is what makes a replication test
        // able to assert on a sequence rather than on a set.
        for (id, entity) in entities.iter() {
            if Some(id) == exclude || !shares_space(id) || !contains(centre, view, entity.chunk()) {
                continue;
            }
            let now = Sent::of(entity);
            match self.known.get(&id) {
                None => update.spawned.push(spawn_of(id, entity)),
                Some(before) => {
                    if now.differs_from(before) {
                        update.moved.push(delta_of(id, entity));
                    }
                    // Asked separately, because it is answered on a different
                    // channel. `differs_from` deliberately does not look at
                    // hands: a hand changing is not a reason to send a
                    // position, and a position changing is not a reason to
                    // resend a hand.
                    if now.hands != before.hands {
                        update.rearmed.push(Armed {
                            id,
                            hands: entity.hands.clone().clone(),
                        });
                    }
                }
            }
            still_visible.insert(id, now);
        }

        // Anything the viewer knew and this pass did not see: it left the
        // cylinder, or it stopped existing. The two are the same message,
        // deliberately — a client cannot do anything different about them, and
        // distinguishing them would mean tracking which entities died as
        // opposed to merely walked away.
        for id in self.known.keys() {
            if !still_visible.contains_key(id) {
                update.despawned.push(*id);
            }
        }

        self.known = still_visible;
        update
    }
}

fn spawn_of(id: EntityId, entity: &Entity) -> Spawn {
    Spawn {
        id,
        transform: entity.transform,
        velocity: entity.velocity,
        model: entity.model.clone(),
        collider: entity.collider,
        item: entity.item.clone().clone(),
        hands: entity.hands.clone().clone(),
        anim: entity.anim,
        nametag: entity.nametag.clone(),
    }
}

fn delta_of(id: EntityId, entity: &Entity) -> Delta {
    Delta {
        id,
        chunk: entity.transform.chunk,
        local: entity.transform.local,
        velocity: entity.velocity.0,
        yaw: quantise_yaw(entity.transform.yaw),
        pitch: quantise_pitch(entity.transform.pitch),
        anim: entity.anim,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEW: ViewDistance = ViewDistance::clamped(2, 1);

    fn at(chunk: ChunkPos) -> Entity {
        Entity::at(Transform::at(chunk, [24.0, 0.0, 24.0]), "test:mob")
    }

    fn home() -> ChunkPos {
        ChunkPos::new(0, 0, 0)
    }

    #[test]
    fn a_viewer_is_told_only_about_bodies_in_their_own_space() {
        // **Chunk interest cannot do this on its own.** Two domains hold a
        // chunk at each coordinate, so a mob in a ship and a mob in the
        // overworld can stand at exactly the same position — and a viewer in
        // one of them must see only their own.
        let mut world = Entities::new();
        let here = world.spawn(at(home()));
        let elsewhere = world.spawn(at(home()));

        let mut tracker = Tracker::new();
        let update = tracker.update(&world, home(), VIEW, None, &|id| id == here);

        let seen: Vec<EntityId> = update.spawned.iter().map(|spawn| spawn.id).collect();
        assert_eq!(
            seen,
            vec![here],
            "a viewer was told about a body standing at the same coordinates in \
             another space"
        );
        assert_eq!(tracker.len(), 1);

        // And the counter-example, so this cannot pass by seeing nothing: with
        // both in the viewer's space, both arrive.
        let mut everything = Tracker::new();
        let both = everything.update(&world, home(), VIEW, None, &|_| true);
        assert_eq!(both.spawned.len(), 2);
        assert!(both.spawned.iter().any(|spawn| spawn.id == elsewhere));
    }

    #[test]
    fn a_body_that_leaves_the_viewers_space_is_despawned_for_them() {
        // Moving between domains has to look like leaving view, or the client
        // keeps drawing a body that is no longer anywhere near it — the same
        // rule as walking out of range, reached a different way.
        let mut world = Entities::new();
        let travelling = world.spawn(at(home()));

        let mut tracker = Tracker::new();
        let arrived = tracker.update(&world, home(), VIEW, None, &|_| true);
        assert_eq!(arrived.spawned.len(), 1);

        let left = tracker.update(&world, home(), VIEW, None, &|_| false);
        assert_eq!(
            left.despawned,
            vec![travelling],
            "a body that moved to another domain was left on the viewer's screen"
        );
        assert!(tracker.is_empty());
    }

    #[test]
    fn an_entity_in_view_is_spawned_once_and_then_left_alone() {
        let mut world = Entities::new();
        let id = world.spawn(at(home()));
        let mut tracker = Tracker::new();

        let first = tracker.update(&world, home(), VIEW, None, &|_| true);
        assert_eq!(first.spawned.len(), 1);
        assert_eq!(first.spawned[0].id, id);
        assert!(first.moved.is_empty() && first.despawned.is_empty());

        // Nothing changed, so nothing is sent. This is the case that decides
        // what a field of settled mobs costs.
        let second = tracker.update(&world, home(), VIEW, None, &|_| true);
        assert!(
            second.is_empty(),
            "a still entity produced traffic: {second:?}"
        );
    }

    #[test]
    fn a_small_movement_is_not_worth_a_packet_and_a_real_one_is() {
        let mut world = Entities::new();
        let id = world.spawn(at(home()));
        let mut tracker = Tracker::new();
        let _ = tracker.update(&world, home(), VIEW, None, &|_| true);

        world.get_mut(id).expect("live").transform.local[0] += MOVE_EPSILON / 2.0;
        assert!(
            tracker
                .update(&world, home(), VIEW, None, &|_| true)
                .is_empty(),
            "a movement under the threshold was sent"
        );

        world.get_mut(id).expect("live").transform.local[0] += MOVE_EPSILON * 2.0;
        let update = tracker.update(&world, home(), VIEW, None, &|_| true);
        assert_eq!(update.moved.len(), 1);
        assert_eq!(update.moved[0].id, id);
    }

    #[test]
    fn a_change_of_animation_is_always_worth_a_packet() {
        // The tag is what the client picks a clip from, so a mob that started
        // swinging without moving must still be reported — a threshold on
        // position alone would swallow it.
        let mut world = Entities::new();
        let id = world.spawn(at(home()));
        let mut tracker = Tracker::new();
        let _ = tracker.update(&world, home(), VIEW, None, &|_| true);

        world.get_mut(id).expect("live").anim = AnimTag::SWING;
        let update = tracker.update(&world, home(), VIEW, None, &|_| true);
        assert_eq!(update.moved.len(), 1);
        assert_eq!(update.moved[0].anim, AnimTag::SWING);
    }

    #[test]
    fn walking_out_of_view_despawns_and_walking_back_spawns_again() {
        let mut world = Entities::new();
        let id = world.spawn(at(home()));
        let mut tracker = Tracker::new();
        assert_eq!(
            tracker
                .update(&world, home(), VIEW, None, &|_| true)
                .spawned
                .len(),
            1
        );

        // Out of the cylinder entirely.
        world.get_mut(id).expect("live").transform.chunk = ChunkPos::new(40, 0, 0);
        let gone = tracker.update(&world, home(), VIEW, None, &|_| true);
        assert_eq!(gone.despawned, vec![id]);
        assert!(tracker.is_empty());

        world.get_mut(id).expect("live").transform.chunk = home();
        let back = tracker.update(&world, home(), VIEW, None, &|_| true);
        assert_eq!(back.spawned.len(), 1, "it did not come back");
        assert!(
            back.moved.is_empty(),
            "it was sent a delta for an entity the client had thrown away"
        );
    }

    #[test]
    fn a_despawned_entity_is_reported_once_and_not_again() {
        let mut world = Entities::new();
        let id = world.spawn(at(home()));
        let mut tracker = Tracker::new();
        let _ = tracker.update(&world, home(), VIEW, None, &|_| true);

        world.despawn(id);
        assert_eq!(
            tracker
                .update(&world, home(), VIEW, None, &|_| true)
                .despawned,
            vec![id]
        );
        assert!(
            tracker
                .update(&world, home(), VIEW, None, &|_| true)
                .is_empty(),
            "the despawn was sent twice"
        );
    }

    #[test]
    fn a_viewer_is_not_told_about_their_own_entity() {
        let mut world = Entities::new();
        let mine = world.spawn(at(home()));
        let theirs = world.spawn(at(home()));
        let mut tracker = Tracker::new();

        let update = tracker.update(&world, home(), VIEW, Some(mine), &|_| true);
        assert_eq!(
            update.spawned.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![theirs]
        );
    }

    #[test]
    fn a_yaw_survives_quantisation_to_within_a_step_and_wraps_the_right_way() {
        // The negative case is the one worth testing: a plain modulo puts
        // `-0.1` at `-0.1` rather than at `0.9`, and a mob that turned slightly
        // left would face backwards.
        for radians in [0.0_f32, 1.0, 3.0, -0.1, -3.0, std::f32::consts::TAU + 0.5] {
            let step = std::f32::consts::TAU / 256.0;
            let back = f32::from(quantise_yaw(radians)) * step;
            let wanted = radians.rem_euclid(std::f32::consts::TAU);
            let error = (back - wanted)
                .abs()
                .min(std::f32::consts::TAU - (back - wanted).abs());
            assert!(
                error <= step,
                "{radians} quantised to {back}, which is {error} away from {wanted}"
            );
        }
    }

    #[test]
    fn a_pitch_past_straight_up_is_clamped_rather_than_wrapped() {
        // Wrapping would make a mob that meant to look at the ground look at
        // the sky, which is the worst possible failure for a thing whose whole
        // job is to seem to be watching you.
        assert_eq!(quantise_pitch(std::f32::consts::PI), 127);
        assert_eq!(quantise_pitch(-std::f32::consts::PI), -127);
        assert_eq!(quantise_pitch(0.0), 0);
    }

    #[test]
    fn two_trackers_over_one_world_agree_about_the_order_they_are_told() {
        // Slot order, so a replication test can assert on a sequence. It also
        // means two servers running the same world send the same messages.
        let mut world = Entities::new();
        for _ in 0..8 {
            world.spawn(at(home()));
        }
        let mut first = Tracker::new();
        let mut second = Tracker::new();
        assert_eq!(
            first
                .update(&world, home(), VIEW, None, &|_| true)
                .spawned
                .iter()
                .map(|s| s.id)
                .collect::<Vec<_>>(),
            second
                .update(&world, home(), VIEW, None, &|_| true)
                .spawned
                .iter()
                .map(|s| s.id)
                .collect::<Vec<_>>()
        );
    }
}
