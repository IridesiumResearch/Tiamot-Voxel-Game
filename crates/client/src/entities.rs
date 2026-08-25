// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Entities as the client holds them, and the buffer that makes them smooth.
//!
//! # Why a client interpolates at all
//!
//! The server is authoritative and ticks at 20 Hz, so an entity's position
//! arrives as a step function: five updates in the time a 100 fps client draws
//! twenty frames. Drawing the newest one each frame gives a mob that teleports
//! five times a second, and no amount of frame rate hides it.
//!
//! So the client draws the world **slightly in the past** and interpolates
//! between the two updates bracketing that moment. Everything it shows has
//! already happened, which is what makes it smooth: there is nothing to guess.
//!
//! # The delay is measured from ARRIVAL, not from the server's clock
//!
//! Every sample is stamped with when this machine received it. The server's
//! tick number rides along, but only to order samples and drop duplicates —
//! never to place them in time.
//!
//! That is deliberate. Using the tick number as a clock would need the two
//! machines' clocks related, and every scheme for relating them (an offset
//! estimate, a smoothed round trip) is a thing that can be wrong — and when it
//! is wrong, it is wrong in the direction of showing entities in a future the
//! client has not been told about yet. Arrival time cannot drift, because it
//! is not measuring anything about the other machine.
//!
//! The cost is that jitter in delivery becomes jitter in playback, which
//! [`INTERPOLATION_DELAY`] is sized to absorb.
//!
//! # This is presentation, and charter rule 4 does not reach it
//!
//! Nothing here feeds simulation. The client's own body is predicted by
//! `crate::predict` through the real physics; entities are things it watches.

use std::collections::BTreeMap;
use std::time::Duration;

use tiamot_core::ChunkPos;
use tiamot_core::proto::{EntityDef, EntityDelta};

/// How far behind the newest update the client draws.
///
/// Two server ticks. One would leave nothing to interpolate the moment a single
/// update was late — the client would be drawing at exactly the newest sample
/// and would have to extrapolate on any jitter at all — and three is a tenth of
/// a second of lag on something a player is looking at.
///
/// The task names ~100 ms, and 100 ms is what two ticks of a 20 Hz server come
/// to. That is not a coincidence worth hiding: the right buffer is a small
/// number of the server's own ticks, and the millisecond figure follows.
pub const INTERPOLATION_DELAY: Duration = Duration::from_millis(100);

/// How many samples one entity keeps.
///
/// Enough to cover the delay several times over, so a burst of late arrivals
/// has something to land in front of. Four is a fifth of a second; past that a
/// sample is older than anything that will ever be drawn.
const HISTORY: usize = 4;

/// Everything the client knows about one entity.
#[derive(Debug, Clone)]
pub struct Entity {
    /// What to draw, or `None` for something invisible.
    pub model: Option<String>,
    /// Footprint and height, in cells.
    pub collider: Option<[f32; 2]>,
    /// The label above it, already the current display name.
    pub nametag: Option<String>,
    /// The stack it looks like, for an item lying on the ground.
    pub item: Option<tiamot_core::proto::StackDef>,
    /// Recent positions, oldest first.
    samples: Vec<Sample>,
}

/// One update, and when it landed.
#[derive(Debug, Clone, Copy)]
struct Sample {
    /// Server tick, for ordering only. See the module docs.
    tick: u64,
    /// When this machine received it.
    at: Duration,
    chunk: ChunkPos,
    local: [f32; 3],
    yaw: u8,
    pitch: i8,
    anim: u8,
}

/// Where an entity should be drawn this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pose {
    /// Chunk half of the position.
    pub chunk: ChunkPos,
    /// Cell offset within that chunk.
    pub local: [f32; 3],
    /// Facing, in radians.
    pub yaw: f32,
    /// Pitch, in radians.
    pub pitch: f32,
    /// Which clip to play.
    pub anim: u8,
}

impl Entity {
    fn from_def(def: &EntityDef, at: Duration) -> Self {
        Self {
            model: def.model.clone(),
            item: def.item,
            collider: def.collider,
            nametag: def.nametag.clone(),
            samples: vec![Sample {
                // A spawn has no tick of its own — it is reliable and arrives
                // once. Zero orders it before every state update, which is
                // exactly right: it IS the oldest thing known about the entity.
                tick: 0,
                at,
                chunk: def.chunk,
                local: def.local,
                yaw: def.yaw,
                pitch: def.pitch,
                anim: def.anim,
            }],
        }
    }

    fn record(&mut self, delta: &EntityDelta, tick: u64, at: Duration) {
        // **Out of order, or a repeat, is dropped.** The state channel is
        // superseding rather than accumulating, so an older sample arriving
        // late carries nothing the buffer does not already have — and letting
        // it in would make the entity walk backwards for one frame.
        if self.samples.last().is_some_and(|last| tick <= last.tick) {
            return;
        }
        self.samples.push(Sample {
            tick,
            at,
            chunk: delta.chunk,
            local: delta.local,
            yaw: delta.yaw,
            pitch: delta.pitch,
            anim: delta.anim,
        });
        if self.samples.len() > HISTORY {
            self.samples.remove(0);
        }
    }

    /// Where to draw it at `now`, or `None` if nothing is known yet.
    ///
    /// `now` is this machine's clock, in the same base the samples were stamped
    /// in. The pose is taken at `now - INTERPOLATION_DELAY`.
    #[must_use]
    pub fn pose(&self, now: Duration) -> Option<Pose> {
        let target = now.saturating_sub(INTERPOLATION_DELAY);
        let newest = self.samples.last()?;

        // Past the end of what is known: hold the newest sample rather than
        // extrapolate. A mob that stopped because its server stopped sending
        // should stand still, not glide away through a wall — and a client that
        // guesses is a client that has to un-guess, visibly, when the truth
        // arrives.
        if target >= newest.at {
            return Some(newest.pose());
        }

        // Before the beginning: the entity has only just been heard of, so
        // there is nothing behind the target to interpolate from.
        let oldest = self.samples.first()?;
        if target <= oldest.at {
            return Some(oldest.pose());
        }

        let pair = self
            .samples
            .windows(2)
            .find(|pair| pair[0].at <= target && target < pair[1].at)?;
        let (before, after) = (&pair[0], &pair[1]);
        let span = after.at.saturating_sub(before.at).as_secs_f32();
        let fraction = if span > 0.0 {
            (target.saturating_sub(before.at).as_secs_f32() / span).clamp(0.0, 1.0)
        } else {
            0.0
        };
        Some(before.blend(after, fraction))
    }

    /// The newest thing known, without interpolating.
    #[must_use]
    pub fn latest(&self) -> Option<Pose> {
        self.samples.last().map(Sample::pose)
    }
}

impl Sample {
    fn pose(&self) -> Pose {
        Pose {
            chunk: self.chunk,
            local: self.local,
            yaw: unquantise_yaw(self.yaw),
            pitch: unquantise_pitch(self.pitch),
            anim: self.anim,
        }
    }

    /// Between this sample and a later one.
    fn blend(&self, other: &Self, fraction: f32) -> Pose {
        // **Interpolated in the OLDER sample's chunk frame.** Two samples
        // either side of a chunk boundary are in different frames, and mixing
        // their `local` parts directly would fling the entity a chunk sideways
        // for one frame — the same class of bug as comparing two entity
        // positions without `Transform::offset_to`.
        let span = tiamot_core::CHUNK_SUBNODES as f32;
        let mut local = [0.0; 3];
        for (axis, slot) in local.iter_mut().enumerate() {
            let chunk_offset = match axis {
                0 => other.chunk.x - self.chunk.x,
                1 => other.chunk.y - self.chunk.y,
                _ => other.chunk.z - self.chunk.z,
            };
            let theirs = chunk_offset as f32 * span + other.local[axis];
            *slot = self.local[axis] + (theirs - self.local[axis]) * fraction;
        }

        Pose {
            chunk: self.chunk,
            local,
            yaw: blend_yaw(self.yaw, other.yaw, fraction),
            pitch: unquantise_pitch(self.pitch)
                + (unquantise_pitch(other.pitch) - unquantise_pitch(self.pitch)) * fraction,
            // A tag does not blend: there is no half-way between walking and
            // swinging. The later one wins as soon as the blend passes it.
            anim: if fraction >= 0.5 {
                other.anim
            } else {
                self.anim
            },
        }
    }
}

/// Radians from a quantised yaw.
#[must_use]
pub fn unquantise_yaw(value: u8) -> f32 {
    f32::from(value) * std::f32::consts::TAU / 256.0
}

/// Radians from a quantised pitch.
#[must_use]
pub fn unquantise_pitch(value: i8) -> f32 {
    f32::from(value) / 127.0 * std::f32::consts::FRAC_PI_2
}

/// Interpolates two quantised yaws the short way round.
///
/// **The short way is the whole point.** A mob turning from 355° to 5° has
/// turned ten degrees, and interpolating the numbers would spin it 350° the
/// other way — once per crossing, for ever, which is the classic way a
/// smoothly-turning character develops a twitch.
///
/// Done in quantised space, where the wrap is a `u8` overflow and the
/// arithmetic cannot be got wrong by an off-by-one at the boundary.
#[must_use]
pub fn blend_yaw(from: u8, to: u8, fraction: f32) -> f32 {
    let forward = to.wrapping_sub(from);
    let backward = from.wrapping_sub(to);
    let step = if forward <= backward {
        f32::from(forward) * fraction
    } else {
        -f32::from(backward) * fraction
    };
    unquantise_yaw(from) + step * std::f32::consts::TAU / 256.0
}

/// Every entity the client has been told about.
#[derive(Debug, Default)]
pub struct Entities {
    held: BTreeMap<u64, Entity>,
}

impl Entities {
    /// An empty world.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many entities are known.
    #[must_use]
    pub fn len(&self) -> usize {
        self.held.len()
    }

    /// Whether none are.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    /// One entity, if it is known.
    #[must_use]
    pub fn get(&self, id: u64) -> Option<&Entity> {
        self.held.get(&id)
    }

    /// Every entity, in id order.
    pub fn iter(&self) -> impl Iterator<Item = (u64, &Entity)> {
        self.held.iter().map(|(id, entity)| (*id, entity))
    }

    /// Records a spawn.
    ///
    /// A spawn for an entity already known REPLACES it. That is the recovery
    /// path: a server whose queue to this client overflowed clears it and
    /// re-sends everything from scratch, and the client must end up with what
    /// the server thinks it has rather than with a merge of two beliefs.
    pub fn spawned(&mut self, entities: &[EntityDef], at: Duration) {
        for def in entities {
            self.held.insert(def.id, Entity::from_def(def, at));
        }
    }

    /// Records a despawn. Unknown ids are ignored.
    pub fn despawned(&mut self, ids: &[u64]) {
        for id in ids {
            self.held.remove(id);
        }
    }

    /// Records a batch of state updates.
    ///
    /// An update for an entity the client has never heard of is dropped. It
    /// means the spawn is still in flight or was lost; inventing an entity from
    /// a delta would give it no model, no collider and no name, and it would
    /// never acquire them.
    pub fn moved(&mut self, tick: u64, entities: &[EntityDelta], at: Duration) {
        for delta in entities {
            if let Some(entity) = self.held.get_mut(&delta.id) {
                entity.record(delta, tick, at);
            }
        }
    }

    /// Forgets everything, for a reconnection.
    pub fn clear(&mut self) {
        self.held.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(id: u64, x: f32) -> EntityDef {
        EntityDef {
            id,
            chunk: ChunkPos::new(0, 0, 0),
            local: [x, 0.0, 0.0],
            velocity: [0.0; 3],
            yaw: 0,
            pitch: 0,
            anim: 0,
            model: Some("engine:humanoid".into()),
            collider: Some([1.8, 5.4]),
            item: None,
            nametag: None,
        }
    }

    fn delta(id: u64, x: f32) -> EntityDelta {
        EntityDelta {
            id,
            chunk: ChunkPos::new(0, 0, 0),
            local: [x, 0.0, 0.0],
            velocity: [0.0; 3],
            yaw: 0,
            pitch: 0,
            anim: 0,
        }
    }

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    #[test]
    fn an_entity_is_drawn_between_the_two_updates_bracketing_the_delay() {
        // The whole mechanism in one test. Updates at 0, 50 and 100 ms put the
        // entity at 0, 10 and 20; drawing at 150 ms means drawing 100 ms ago,
        // which is the update at 50 — position 10, exactly, no blending needed.
        let mut world = Entities::new();
        world.spawned(&[def(1, 0.0)], ms(0));
        world.moved(1, &[delta(1, 10.0)], ms(50));
        world.moved(2, &[delta(1, 20.0)], ms(100));

        let pose = world.get(1).expect("known").pose(ms(150)).expect("pose");
        assert!(
            (pose.local[0] - 10.0).abs() < 0.001,
            "drew at {} rather than at the 100 ms-old sample",
            pose.local[0]
        );

        // And halfway between two samples, halfway between two positions.
        let pose = world.get(1).expect("known").pose(ms(175)).expect("pose");
        assert!(
            (pose.local[0] - 15.0).abs() < 0.001,
            "drew at {} rather than halfway",
            pose.local[0]
        );
    }

    #[test]
    fn a_client_that_has_run_out_of_updates_holds_still_rather_than_gliding_on() {
        // Extrapolating past the newest sample is a guess, and a guess has to
        // be visibly un-guessed when the truth arrives. A mob whose server
        // stopped talking should stand still, not walk through a wall.
        let mut world = Entities::new();
        world.spawned(&[def(1, 0.0)], ms(0));
        world.moved(1, &[delta(1, 10.0)], ms(50));

        let far_future = world.get(1).expect("known").pose(ms(5_000)).expect("pose");
        assert!(
            (far_future.local[0] - 10.0).abs() < 0.001,
            "the entity glided to {} with nothing to go on",
            far_future.local[0]
        );
    }

    #[test]
    fn an_update_that_arrives_late_and_out_of_order_is_dropped() {
        // The state channel supersedes rather than accumulates, so an older
        // sample carries nothing new — and letting it in makes the entity walk
        // backwards for a frame.
        let mut world = Entities::new();
        world.spawned(&[def(1, 0.0)], ms(0));
        world.moved(5, &[delta(1, 50.0)], ms(50));
        world.moved(3, &[delta(1, 30.0)], ms(60));

        let latest = world.get(1).expect("known").latest().expect("pose");
        assert!(
            (latest.local[0] - 50.0).abs() < 0.001,
            "the stale update was accepted: entity is at {}",
            latest.local[0]
        );
    }

    #[test]
    fn interpolation_across_a_chunk_boundary_does_not_fling_the_entity() {
        // Two samples either side of a seam are in different frames. Mixing
        // their `local` parts directly sends the entity a chunk sideways for
        // one frame — the same class of bug as comparing two entity positions
        // without `offset_to`.
        let span = tiamot_core::CHUNK_SUBNODES as f32;
        let mut world = Entities::new();
        world.spawned(&[def(1, span - 1.0)], ms(0));
        world.moved(
            1,
            &[EntityDelta {
                chunk: ChunkPos::new(1, 0, 0),
                local: [1.0, 0.0, 0.0],
                ..delta(1, 0.0)
            }],
            ms(50),
        );

        // Halfway between cell 47 of chunk 0 and cell 1 of chunk 1 — which is
        // two cells apart, so halfway is one cell along.
        let pose = world.get(1).expect("known").pose(ms(125)).expect("pose");
        assert_eq!(pose.chunk, ChunkPos::new(0, 0, 0));
        assert!(
            (pose.local[0] - span).abs() < 0.001,
            "the entity was drawn at {} rather than one cell over the seam",
            pose.local[0]
        );
    }

    #[test]
    fn a_yaw_crossing_the_wrap_turns_the_short_way() {
        // 355 degrees to 5 degrees is a ten-degree turn. Interpolating the
        // numbers spins it 350 degrees the other way, once per crossing, for
        // ever — the classic way a smooth character develops a twitch.
        let from = 252_u8; // ~354 degrees
        let to = 4_u8; // ~5.6 degrees
        let halfway = blend_yaw(from, to, 0.5);
        let step = std::f32::consts::TAU / 256.0;
        // Eight steps forward from 252 is 256, which wraps to 0.
        let wanted = unquantise_yaw(from) + 4.0 * step;
        assert!(
            (halfway - wanted).abs() < step,
            "halfway came out {halfway}, not near {wanted}"
        );
    }

    #[test]
    fn a_delta_for_an_unknown_entity_does_not_invent_one() {
        // The spawn is reliable and may still be in flight. An entity invented
        // from a delta has no model, no collider and no name, and would never
        // acquire them.
        let mut world = Entities::new();
        world.moved(1, &[delta(7, 1.0)], ms(0));
        assert!(world.is_empty(), "a delta conjured an entity");
    }

    #[test]
    fn a_second_spawn_replaces_rather_than_merges() {
        // The recovery path: a server whose queue to this client overflowed
        // clears it and re-sends from scratch, and the client has to end up
        // with what the server believes rather than a mixture.
        let mut world = Entities::new();
        world.spawned(&[def(1, 0.0)], ms(0));
        world.moved(9, &[delta(1, 99.0)], ms(50));

        world.spawned(&[def(1, 5.0)], ms(100));
        let latest = world.get(1).expect("known").latest().expect("pose");
        assert!(
            (latest.local[0] - 5.0).abs() < 0.001,
            "the re-spawn was merged with the old history: entity is at {}",
            latest.local[0]
        );
    }

    #[test]
    fn despawning_an_unknown_id_is_not_an_error() {
        let mut world = Entities::new();
        world.despawned(&[1, 2, 3]);
        assert!(world.is_empty());
    }
}
