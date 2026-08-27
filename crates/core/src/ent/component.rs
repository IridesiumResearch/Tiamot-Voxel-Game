// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! What an entity is made of.
//!
//! # These are fields, not a component registry
//!
//! The engine-defined set is fixed and known at compile time, which is the
//! finding that decided [`docs/ecs-verdict.md`](../../../../docs/ecs-verdict.md):
//! an ECS earns its archetypes when combinations are discovered at runtime, and
//! here they are not. So each of these is a field on [`super::Entity`] —
//! `Option` where an entity may genuinely lack it, plain where it may not.
//!
//! Mods attach their own state, and it is deliberately ONE more field: a handle
//! into the script VM's registry. Every mod's table shares it, because from the
//! engine's side a Lua table is opaque and one opaque thing is the same as
//! another (charter rule 1 — the engine holds mechanisms, not meanings).
//!
//! # Serialisation
//!
//! Everything here round-trips through `postcard` with the entity's chunk, so
//! **enum variants are position-encoded**: appending is safe, inserting or
//! reordering silently reinterprets every saved world. See
//! [`crate::persist::codec`].

use crate::coords::ChunkPos;
use crate::identity::PlayerUuid;

/// Where an entity is, as charter rule 7 requires.
///
/// The pair `(ChunkPos, f32 local)` is a floating origin: world-space `f32` is
/// never accumulated, so precision does not decay 60,000 blocks from the origin.
/// `local` is in **sub-node cells**, `0..48`, which is the unit [`crate::phys`]
/// works in — the same choice, for the same reason, as
/// [`crate::proto::ServerMessage::PlayerState`].
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Transform {
    /// The chunk `local` is measured from.
    pub chunk: ChunkPos,
    /// Cell offset within that chunk. The feet, centred on the footprint.
    pub local: [f32; 3],
    /// Which way the entity is facing, in radians about the vertical axis.
    ///
    /// **Presentation, not simulation.** Nothing in the physics reads it: a
    /// body is a box and a box has no heading. It exists so a client can point
    /// a humanoid the way it is walking and so a mod can say which way a mob
    /// looks without inventing its own component.
    pub yaw: f32,
    /// How far up or down it is looking, in radians.
    pub pitch: f32,
}

impl Transform {
    /// At rest at a cell offset in a chunk, facing along +z.
    #[must_use]
    pub const fn at(chunk: ChunkPos, local: [f32; 3]) -> Self {
        Self {
            chunk,
            local,
            yaw: 0.0,
            pitch: 0.0,
        }
    }

    /// The offset from this transform to another, in cells.
    ///
    /// **The only correct way to compare two entity positions**, because two
    /// transforms anchored to different chunks are in different frames and
    /// subtracting their `local` parts compares nothing. Deterministic: the
    /// chunk difference is integer, and the one float subtraction per axis is
    /// in the allowed subset (charter rule 4).
    #[must_use]
    pub fn offset_to(&self, other: &Self) -> [f32; 3] {
        let span = crate::CHUNK_SUBNODES as f32;
        let axis = |index: usize, mine: i32, theirs: i32| {
            (theirs - mine) as f32 * span + other.local[index] - self.local[index]
        };
        [
            axis(0, self.chunk.x, other.chunk.x),
            axis(1, self.chunk.y, other.chunk.y),
            axis(2, self.chunk.z, other.chunk.z),
        ]
    }

    /// Squared distance to another transform, in cells.
    ///
    /// Squared because the comparison a caller wants is against a radius, and
    /// squaring the radius once beats a square root per entity. `sqrt` is in
    /// the deterministic subset, so this is a speed choice rather than a
    /// correctness one.
    #[must_use]
    pub fn distance_squared(&self, other: &Self) -> f32 {
        let offset = self.offset_to(other);
        offset[0] * offset[0] + offset[1] * offset[1] + offset[2] * offset[2]
    }

    /// From world block coordinates, which is how a mod says where something is.
    ///
    /// # Mods do not have chunk frames, and must not
    ///
    /// Charter rule 7's `(chunk, local)` pairing is an engine concern: it exists
    /// so world-space `f32` is never accumulated. A mod that had to know about
    /// it would be a mod that gets it wrong at 60,000 blocks out, and every mod
    /// would have to get it right separately. So the API speaks plain world
    /// blocks — one block is one yard (charter rule 5) — and the conversion
    /// lives here, once.
    ///
    /// `f64` in, because that is what a Lua number is. The arithmetic is
    /// `floor`, subtract and multiply, all inside the deterministic subset.
    #[must_use]
    pub fn from_world(x: f64, y: f64, z: f64) -> Self {
        let span = f64::from(crate::CHUNK_BLOCKS);
        let cells = f64::from(crate::SUBNODES_PER_AXIS);
        let axis = |value: f64| {
            let chunk = crate::detgen::floor_to_i32((value / span) as f32);
            let local = (value - f64::from(chunk) * span) * cells;
            (chunk, local as f32)
        };
        let (cx, lx) = axis(x);
        let (cy, ly) = axis(y);
        let (cz, lz) = axis(z);
        Self::at(ChunkPos::new(cx, cy, cz), [lx, ly, lz])
    }

    /// Back to world block coordinates.
    #[must_use]
    pub fn to_world(&self) -> [f64; 3] {
        let span = f64::from(crate::CHUNK_BLOCKS);
        let cells = f64::from(crate::SUBNODES_PER_AXIS);
        let axis = |chunk: i32, local: f32| f64::from(chunk) * span + f64::from(local) / cells;
        [
            axis(self.chunk.x, self.local[0]),
            axis(self.chunk.y, self.local[1]),
            axis(self.chunk.z, self.local[2]),
        ]
    }

    /// The block this entity's feet are in.
    ///
    /// `floor_to_i32` rather than `f32::floor`: the latter lowers to libm
    /// without SSE4.1 and is on charter rule 4's banned list. The clippy
    /// `disallowed-methods` gate caught this one, which is the gate working.
    #[must_use]
    pub fn block(&self) -> crate::BlockPos {
        let corner = crate::BlockPos::from_chunk_corner(self.chunk);
        let per_axis = crate::SUBNODES_PER_AXIS as f32;
        let floor = |value: f32| crate::detgen::floor_to_i32(value / per_axis);
        crate::BlockPos::new(
            corner.x + floor(self.local[0]),
            corner.y + floor(self.local[1]),
            corner.z + floor(self.local[2]),
        )
    }
}

/// How fast an entity is moving, in cells per tick.
#[derive(Debug, Clone, Copy, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct Velocity(pub [f32; 3]);

/// The box an entity occupies.
///
/// [`crate::phys::Shape`] itself, not a copy of it. The player has been
/// colliding through that type since Task 09, and charter rule 2 means there
/// had better be exactly one answer to "how big is the thing being simulated" —
/// a mob with its own box type would be the beginning of a second physics.
pub type Collider = crate::phys::Shape;

/// The model the engine ships, and the one thing that is not mod content.
///
/// Players have to be drawn as something before any mod has loaded, so the
/// humanoid rig is the engine's. Mods may use it too — see the task's
/// `engine:humanoid`.
pub const HUMANOID_MODEL: &str = "engine:humanoid";

/// A camera's yaw, as the yaw of the BODY looking through it.
///
/// # Two conventions, and they are not the same one
///
/// A figure faces `(sin yaw, cos yaw)` — that is what the skinned vertex stage
/// does with a heading, and what a mob writing `atan2(dx, dz)` produces. A
/// CAMERA looks along `(−sin yaw, cos yaw)`, because the world is right-handed
/// with `+z` north, which makes east `−x`. The two therefore count in opposite
/// directions, and the conversion is the negation.
///
/// **Getting it wrong is invisible until you turn**: at yaw zero both point
/// north and agree exactly. It has now been got wrong twice — once for the
/// local player's own figure in third person, and once for the mirrored body
/// every OTHER player sees, which stored the camera's yaw unconverted and so
/// faced the mirror image of where its owner was looking.
///
/// It lives in `core` rather than in the client so that both ends convert the
/// same way, once. It is not rendering: it is which way round an angle counts.
#[must_use]
pub const fn figure_yaw(camera_yaw: f32) -> f32 {
    -camera_yaw
}

/// What an entity is doing, as a tag.
///
/// **The server never touches animation maths.** It says "walking" and the
/// client picks a clip and advances its time. That split is what keeps skeletal
/// animation out of the deterministic simulation entirely: interpolating a
/// joint is transcendental work, and charter rule 4 explicitly does not reach
/// presentation.
///
/// A `u8` rather than an enum so a mod can register its own tags for its own
/// models without an engine change. The built-ins below are the clips the
/// engine's humanoid ships with.
///
/// # Not persisted
///
/// What a mob was doing when its chunk unloaded is not world state — it is a
/// frame of presentation, and an entity thawing a week later has no business
/// resuming a swing. [`super::Entity`] skips this field on the way to disk and
/// it comes back [`Self::IDLE`].
///
/// That is also what keeps charter rule 8 out of this type. Numeric ids are
/// per-session; a mod-registered tag would be numbered positionally at load
/// time, so writing one to disk would mean a world's saved mobs silently
/// changing what they were doing when a mod's load order changed — the fluid-id
/// defect, in a smaller place. Not writing it at all is the whole fix.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct AnimTag(pub u8);

impl AnimTag {
    /// Standing still.
    pub const IDLE: Self = Self(0);
    /// Moving at a walk.
    pub const WALK: Self = Self(1);
    /// Moving at a run.
    pub const RUN: Self = Self(2);
    /// Swinging whatever it is holding.
    pub const SWING: Self = Self(3);
    /// In a fluid, above its own feet.
    pub const SWIM: Self = Self(4);
    /// Crouched.
    pub const SNEAK: Self = Self(5);
}

impl Default for AnimTag {
    fn default() -> Self {
        Self::IDLE
    }
}

/// Hit points.
///
/// The engine holds the number and nothing else — no damage types, no
/// resistances, no death behaviour. What running out MEANS is a mod's business
/// (charter rule 1); the engine only fires the hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Health {
    /// Current hit points.
    pub current: u32,
    /// The most it can have.
    pub max: u32,
}

impl Health {
    /// Full health at `max`.
    #[must_use]
    pub const fn full(max: u32) -> Self {
        Self { current: max, max }
    }

    /// Whether it has run out.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.current == 0
    }
}

/// The label drawn above an entity.
///
/// # Why a player variant exists
///
/// Charter rule 13: display names are a per-server claim bound to a UUID, and
/// **the UUID is the identity**. A mod that wants to label something with a
/// player's name must therefore not store the name — the player may rebind it,
/// and a stored copy would go stale and, worse, would be the thing a later
/// lookup keyed on.
///
/// So the engine stores the UUID and resolves the CURRENT name at send time.
/// That is a mechanism rather than content: it is the only way a mod can hold a
/// name-shaped thing without breaking rule 13, and leaving it out would push
/// every mod that wants one into doing it wrong.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Nametag {
    /// A literal label, whatever the mod wants it to say.
    Text(String),
    /// Whatever this player is currently called on this server.
    Player(PlayerUuid),
}

/// Who an entity belongs to.
///
/// What a body has in its hands: main, then off.
///
/// **Two slots and not the whole hotbar.** The client draws hands, so hands are
/// what the wire has to carry — and every extra slot is a stack of somebody
/// else's inventory sent to everybody who can see them. The whole bar was
/// authorised and is not needed.
///
/// A `None` is an empty hand, which is drawn as an empty hand rather than as
/// nothing: an arm is still there.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Hands {
    /// The stack in the main hand.
    pub main: Option<crate::inventory::Stack>,
    /// The stack in the off hand.
    pub off: Option<crate::inventory::Stack>,
}

impl Hands {
    /// Whether both hands are empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.main.is_none() && self.off.is_none()
    }
}

/// A UUID, never a name (charter rule 13). Survives the owner renaming
/// themselves, going offline, and rotating their key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Owner(pub PlayerUuid);

#[cfg(test)]
mod yaw_tests {
    use super::*;

    #[test]
    fn a_body_faces_the_way_the_camera_looks() {
        // **The conversion, checked against both conventions rather than
        // asserted.** A figure faces `(sin yaw, cos yaw)` — what the skinned
        // vertex stage does with a heading — and a camera looks along
        // `(−sin yaw, cos yaw)`, because the world is right-handed with `+z`
        // north and east is `−x`.
        //
        // Zero is deliberately not the only case: at yaw zero both point north
        // and a missing conversion is invisible. That is how it shipped wrong
        // twice — once on the local player's own figure, once on the mirrored
        // body every other player sees.
        for degrees in [0.0_f32, 45.0, 90.0, 180.0, 270.0, -30.0] {
            let camera = degrees.to_radians();
            let body = figure_yaw(camera);

            let looking = (
                -crate::detgen::trig::sin(camera),
                crate::detgen::trig::cos(camera),
            );
            let facing = (
                crate::detgen::trig::sin(body),
                crate::detgen::trig::cos(body),
            );
            assert!(
                (facing.0 - looking.0).abs() < 1e-5 && (facing.1 - looking.1).abs() < 1e-5,
                "at {degrees}° the camera looks {looking:?} and the body faces {facing:?}"
            );
        }

        // And the counter-example: passing the camera's yaw straight through —
        // which is what the player mirror did — disagrees the moment you turn.
        let camera = 90.0_f32.to_radians();
        assert!(
            (crate::detgen::trig::sin(camera) - -crate::detgen::trig::sin(camera)).abs() > 1.0,
            "the unconverted value agrees, so this test proves nothing"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_transforms_in_different_chunks_measure_the_distance_between_them() {
        // **The bug this exists to make impossible.** Two entities anchored to
        // different chunks are in different frames, so subtracting their local
        // parts measures nothing — and it measures nothing *plausibly*, which is
        // worse: an entity one chunk east reads as being right beside you.
        let here = Transform::at(ChunkPos::new(0, 0, 0), [24.0, 0.0, 24.0]);
        let east = Transform::at(ChunkPos::new(1, 0, 0), [24.0, 0.0, 24.0]);

        // `.abs() < EPSILON` rather than `assert_eq!`: the values really are
        // exact here, but `clippy::float_cmp` is deny-level in this crate and a
        // test exemption is a precedent it does not want. Same convention as
        // `phys::tests`.
        let offset = here.offset_to(&east);
        let want = [crate::CHUNK_SUBNODES as f32, 0.0, 0.0];
        for axis in 0..3 {
            assert!(
                (offset[axis] - want[axis]).abs() < f32::EPSILON,
                "one chunk east should be one chunk's worth of cells away, got {offset:?}"
            );
        }
        // And the naive answer, so the test says what it is protecting against.
        assert!(
            offset[0].abs() > 1.0,
            "subtracting the local parts would have said zero"
        );
    }

    #[test]
    fn distance_is_symmetric_and_squared() {
        let a = Transform::at(ChunkPos::new(-3, 0, 5), [1.0, 2.0, 3.0]);
        let b = Transform::at(ChunkPos::new(2, 1, 5), [4.0, 6.0, 3.0]);
        assert!(
            (a.distance_squared(&b) - b.distance_squared(&a)).abs() < f32::EPSILON,
            "distance is not symmetric"
        );

        let near = Transform::at(ChunkPos::new(0, 0, 0), [0.0, 0.0, 0.0]);
        let far = Transform::at(ChunkPos::new(0, 0, 0), [3.0, 4.0, 0.0]);
        assert!(
            (near.distance_squared(&far) - 25.0).abs() < f32::EPSILON,
            "3-4-5 triangle, squared, so 25 and not 5: got {}",
            near.distance_squared(&far)
        );
    }

    #[test]
    fn a_transform_reports_the_block_its_feet_are_in() {
        let chunk = ChunkPos::new(2, 0, -1);
        let corner = crate::BlockPos::from_chunk_corner(chunk);
        // Cell 47 is the last cell of the chunk, so block 15 of it.
        let transform = Transform::at(chunk, [47.0, 0.5, 3.0]);
        assert_eq!(
            transform.block(),
            crate::BlockPos::new(corner.x + 15, corner.y, corner.z + 1)
        );
    }

    #[test]
    fn world_coordinates_round_trip_through_the_chunk_frame() {
        // The conversion a mod's every position goes through. It must survive
        // the round trip, and it must work at a NEGATIVE coordinate — where a
        // truncating divide would put a position in the wrong chunk and a
        // `rem` would give it a negative local offset.
        for (x, y, z) in [
            (0.0, 0.0, 0.0),
            (10.5, 64.0, -3.25),
            (-0.5, -1.0, -16.0),
            (-1000.75, 200.5, 999.25),
        ] {
            let transform = Transform::from_world(x, y, z);
            let [bx, by, bz] = transform.to_world();
            let close = |a: f64, b: f64| (a - b).abs() < 1.0 / 48.0;
            assert!(
                close(bx, x) && close(by, y) && close(bz, z),
                "({x}, {y}, {z}) came back as ({bx}, {by}, {bz})"
            );
            let span = crate::CHUNK_BLOCKS as f32 * crate::SUBNODES_PER_AXIS as f32;
            for axis in 0..3 {
                assert!(
                    transform.local[axis] >= 0.0 && transform.local[axis] < span,
                    "({x}, {y}, {z}) produced a local offset outside its own chunk: {:?}",
                    transform.local
                );
            }
        }
    }

    #[test]
    fn a_nametag_bound_to_a_player_stores_the_uuid_and_not_the_name() {
        // Charter rule 13 in one assertion: there is nowhere in this type to
        // put a name, so a mod cannot accidentally key on one.
        let uuid = PlayerUuid::from_bytes([7; 32]);
        let tag = Nametag::Player(uuid);
        let round_tripped: Nametag =
            postcard::from_bytes(&postcard::to_allocvec(&tag).expect("encode")).expect("decode");
        assert_eq!(round_tripped, tag);
    }
}
