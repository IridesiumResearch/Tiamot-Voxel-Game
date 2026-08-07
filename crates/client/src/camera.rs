// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The camera, and the floating origin that keeps it precise.
//!
//! # The problem floating origin solves
//!
//! An `f32` has 24 bits of mantissa. At a world coordinate of 50,000 blocks
//! that leaves roughly 1/256 of a block of precision — about 0.004 blocks.
//! A sub-node is 1/3 of a block, so the quantisation error is over 1% of the
//! smallest thing this engine renders. The symptom is not a wrong picture but a
//! *moving* one: vertices snap between representable values as the camera
//! moves, and the whole world shimmers.
//!
//! Charter rule 7: authoritative positions are `(i32 chunk, f32 local)`, and
//! world-space `f32` is never accumulated.
//!
//! # How the rendering side works
//!
//! Mesh vertices are **chunk-local** — sub-node coordinates in `0..=48`, which
//! `f32` represents exactly. Per draw, the transform is
//! `(chunk_pos − camera_chunk) × 16`, computed in `f64` on the CPU and narrowed
//! to `f32`. That difference is bounded by the view distance, not by where the
//! camera is: at view distance 8 it never exceeds 128 blocks, where `f32` has
//! about 1e-5 of precision. Ten thousand times more than a sub-node needs.
//!
//! The property is therefore checkable without a GPU, and
//! [`tests`] does: **the draw offset's magnitude depends on the view distance
//! and nothing else.** Rendering at the origin and rendering at 50,000 blocks
//! push identical numbers through the pipeline.
//!
//! # Charter rule 4 does not reach here
//!
//! Rule 4 is explicit: the deterministic float subset is required for worldgen,
//! the simulation tick, and the CI hash gate, and is "explicitly NOT required
//! for rendering, audio, UI layout, camera smoothing, or client-side
//! interpolation — do not tax presentation code with these rules."
//!
//! The workspace's `disallowed-methods` lint is nonetheless workspace-wide,
//! which is the safer default: a lint that only fired where someone remembered
//! to enable it would miss the case that mattered. So the trigonometry below
//! carries a targeted `allow` with its reason attached, rather than the lint
//! being loosened for everyone.

use glam::{Mat4, Vec3};
use tiamot_core::{CHUNK_BLOCKS, ChunkPos, SUBNODES_PER_AXIS};

/// Blocks per chunk, as `f64` for the offset arithmetic.
const CHUNK_SPAN: f64 = CHUNK_BLOCKS as f64;

/// A camera position that never accumulates world-space `f32`.
///
/// The split is the point: `chunk` carries the magnitude, `local` carries the
/// precision, and `local` is renormalised into `chunk` whenever it leaves the
/// chunk. A single `f32` triple would lose precision as the player walked; a
/// single `f64` triple would keep it and then throw it away at the GPU
/// boundary.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Position {
    /// Which chunk the camera is in.
    pub chunk: ChunkPos,
    /// Where inside that chunk, in blocks, each component in `0.0..16.0`.
    pub local: Vec3,
}

impl Default for Position {
    fn default() -> Self {
        Self {
            chunk: ChunkPos::new(0, 0, 0),
            local: Vec3::new(8.0, 8.0, 8.0),
        }
    }
}

impl Position {
    /// A position from absolute world-block coordinates.
    ///
    /// Takes `f64` because the caller has a world coordinate and must not have
    /// rounded it to `f32` on the way here — which is the mistake this whole
    /// module exists to prevent.
    #[must_use]
    pub fn from_world(x: f64, y: f64, z: f64) -> Self {
        let chunk = ChunkPos::new(
            div_floor(x, CHUNK_SPAN),
            div_floor(y, CHUNK_SPAN),
            div_floor(z, CHUNK_SPAN),
        );
        Self {
            chunk,
            local: Vec3::new(
                rem_floor_f32(x, CHUNK_SPAN),
                rem_floor_f32(y, CHUNK_SPAN),
                rem_floor_f32(z, CHUNK_SPAN),
            ),
        }
    }

    /// Absolute world coordinates, in blocks.
    ///
    /// For display and for debugging only. Nothing in the render path should
    /// need this — if something does, it is about to accumulate a world-space
    /// float.
    #[must_use]
    pub fn to_world(self) -> (f64, f64, f64) {
        (
            f64::from(self.chunk.x) * CHUNK_SPAN + f64::from(self.local.x),
            f64::from(self.chunk.y) * CHUNK_SPAN + f64::from(self.local.y),
            f64::from(self.chunk.z) * CHUNK_SPAN + f64::from(self.local.z),
        )
    }

    /// The offset from here to a world point, in blocks, ready for a shader.
    ///
    /// The same trick as [`Position::chunk_offset`] and for the same reason:
    /// the subtraction happens in `f64`, where a world coordinate is exact, and
    /// only the small difference is narrowed to `f32`. Narrowing first would
    /// quantise a position fifty thousand blocks out to steps a body is smaller
    /// than.
    #[must_use]
    pub fn offset_to(self, world: [f64; 3]) -> [f32; 3] {
        let (x, y, z) = self.to_world();
        [
            (world[0] - x) as f32,
            (world[1] - y) as f32,
            (world[2] - z) as f32,
        ]
    }

    /// Moves by a delta in blocks, renormalising into the chunk grid.
    ///
    /// Renormalising on every move is what keeps `local` small. Letting it
    /// drift and fixing it up occasionally would work until someone walked in
    /// one direction for a while, which is a thing players do.
    pub fn translate(&mut self, delta: Vec3) {
        self.local += delta;
        self.renormalise();
    }

    /// Carries any whole chunks out of `local` and into `chunk`.
    pub fn renormalise(&mut self) {
        for axis in 0..3 {
            let value = f64::from(self.local[axis]);
            let carry = div_floor(value, CHUNK_SPAN);
            if carry != 0 {
                match axis {
                    0 => self.chunk.x = self.chunk.x.saturating_add(carry),
                    1 => self.chunk.y = self.chunk.y.saturating_add(carry),
                    _ => self.chunk.z = self.chunk.z.saturating_add(carry),
                }
                self.local[axis] = rem_floor_f32(value, CHUNK_SPAN);
            }
        }
    }

    /// The offset, in blocks, from this position to a chunk's origin.
    ///
    /// **This is the floating origin.** Computed in `f64` from the integer
    /// chunk difference, so the result depends on the *relative* position and
    /// not on how far from the world origin either of them is.
    #[must_use]
    pub fn chunk_offset(self, chunk: ChunkPos) -> Vec3 {
        // The subtraction is on i32 chunk coordinates promoted to f64: exact,
        // and impossible to lose precision in. Multiplying by 16 afterwards is
        // exact too, since 16 is a power of two.
        let dx = (f64::from(chunk.x) - f64::from(self.chunk.x)) * CHUNK_SPAN;
        let dy = (f64::from(chunk.y) - f64::from(self.chunk.y)) * CHUNK_SPAN;
        let dz = (f64::from(chunk.z) - f64::from(self.chunk.z)) * CHUNK_SPAN;

        // Only now, with the magnitude already small, is it narrowed to f32.
        Vec3::new(
            (dx - f64::from(self.local.x)) as f32,
            (dy - f64::from(self.local.y)) as f32,
            (dz - f64::from(self.local.z)) as f32,
        )
    }
}

/// Euclidean division that floors toward negative infinity.
///
/// `as i32` truncates toward zero, which puts every position with a negative
/// coordinate in the wrong chunk — and the bug looks like a one-chunk seam that
/// only exists west and below the origin.
#[expect(
    clippy::float_cmp,
    reason = "the exact comparison IS the test: it asks whether truncation changed the value, \
              which a tolerance would answer wrongly for quotients within the tolerance of an \
              integer"
)]
fn div_floor(value: f64, divisor: f64) -> i32 {
    let quotient = value / divisor;
    // No `floor`: the workspace's determinism lint bans it, and this is a
    // narrowing conversion anyway. Truncation plus a correction is exact for
    // the range chunk coordinates live in.
    let truncated = quotient as i64;
    let floored = if quotient < 0.0 && (truncated as f64) != quotient {
        truncated - 1
    } else {
        truncated
    };
    i32::try_from(floored).unwrap_or(if floored < 0 { i32::MIN } else { i32::MAX })
}

/// Remainder matching [`div_floor`], always in `0.0..divisor`.
fn rem_floor(value: f64, divisor: f64) -> f64 {
    let remainder = value - f64::from(div_floor(value, divisor)) * divisor;
    remainder.clamp(0.0, divisor)
}

/// The remainder as an `f32` **strictly below** `divisor`.
///
/// Clamping in `f64` and narrowing afterwards does not work: `16.0 - f64::EPSILON`
/// rounds to exactly `16.0` as an `f32`, so the clamp is undone by the very
/// conversion it was protecting. The local coordinate then equals a full chunk,
/// which means "the next chunk" and shows up as a one-frame pop.
///
/// Caught by `local_coordinates_stay_inside_the_chunk` after ~1000 moves.
fn rem_floor_f32(value: f64, divisor: f64) -> f32 {
    let remainder = rem_floor(value, divisor) as f32;
    let limit = divisor as f32;
    if remainder >= limit {
        // The largest f32 strictly below the chunk span.
        f32::from_bits(limit.to_bits() - 1)
    } else {
        remainder
    }
}

/// A free-fly camera.
///
/// The real character controller is Task 09; this exists to look at the world.
#[derive(Debug, Clone, Copy)]
pub struct Camera {
    /// Where the camera is.
    pub position: Position,
    /// Yaw in radians, 0 looking along +z, increasing to turn right.
    ///
    /// Right is east: +z is north and +x is west, because the world is
    /// right-handed with +y up. See [`Camera::forward`].
    pub yaw: f32,
    /// Pitch in radians, clamped away from straight up and down.
    pub pitch: f32,
    /// Vertical field of view, radians.
    pub fov_y: f32,
    /// Near plane, in blocks.
    pub near: f32,
    /// Far plane, in blocks.
    pub far: f32,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            position: Position::default(),
            yaw: 0.0,
            pitch: 0.0,
            // 70 degrees. Wide enough not to feel like a telescope, narrow
            // enough not to distort at the edges.
            fov_y: 70.0_f32.to_radians(),
            near: 0.05,
            // Far enough for the default view distance of 8 chunks with room
            // to spare; the frustum cull does the real work.
            far: 1000.0,
        }
    }
}

/// How close to straight up or down the pitch may get, in radians.
///
/// Exactly vertical makes the view matrix degenerate — the forward vector
/// becomes parallel to "up" and the basis collapses — so the camera spins
/// wildly for one frame. Every engine clamps this; the value is arbitrary and
/// the clamp is not.
const PITCH_LIMIT: f32 = std::f32::consts::FRAC_PI_2 - 0.001;

impl Camera {
    /// The direction the camera is looking.
    #[expect(
        clippy::disallowed_methods,
        reason = "charter rule 4 explicitly exempts camera maths from the deterministic float \
                  subset; sin_cos here decides where to point a camera, not what the world is"
    )]
    #[must_use]
    pub fn forward(&self) -> Vec3 {
        let (sin_pitch, cos_pitch) = self.pitch.sin_cos();
        let (sin_yaw, cos_yaw) = self.yaw.sin_cos();
        // The negated x is not a fudge, and removing it inverts mouse-look.
        // The world is right-handed with +y up and +z north, so +x is
        // `up × north` = WEST, and east is −x. Yaw increases north → east
        // (`compass` names the sectors in that order), so a growing yaw must
        // swing the forward vector toward −x. Written with +sin_yaw it swung
        // toward +x instead: the view turned left when the mouse went right,
        // and the HUD called west "east".
        Vec3::new(-cos_pitch * sin_yaw, sin_pitch, cos_pitch * cos_yaw).normalize()
    }

    /// The camera's right vector.
    #[must_use]
    pub fn right(&self) -> Vec3 {
        self.forward().cross(Vec3::Y).normalize_or_zero()
    }

    /// Applies mouse movement, in radians.
    pub fn look(&mut self, delta_yaw: f32, delta_pitch: f32) {
        self.yaw = (self.yaw + delta_yaw).rem_euclid(std::f32::consts::TAU);
        self.pitch = (self.pitch + delta_pitch).clamp(-PITCH_LIMIT, PITCH_LIMIT);
    }

    /// Moves relative to where the camera is facing.
    pub fn fly(&mut self, forward: f32, right: f32, up: f32) {
        let delta = self.forward() * forward + self.right() * right + Vec3::Y * up;
        self.position.translate(delta);
    }

    /// The view matrix, with the camera at the origin.
    ///
    /// The camera is **always at the origin** in this space — that is what
    /// floating origin means. Geometry is moved to it, not the other way round.
    #[must_use]
    pub fn view(&self) -> Mat4 {
        glam::camera::rh::view::look_to_mat4(Vec3::ZERO, self.forward(), Vec3::Y)
    }

    /// The projection matrix for an aspect ratio.
    #[must_use]
    pub fn projection(&self, aspect: f32) -> Mat4 {
        // Reverse-Z would be the better choice for depth precision at long
        // range; that is a Task 15b concern, and changing it later does not
        // touch anything outside this function.
        glam::camera::rh::proj::directx::perspective(
            self.fov_y,
            aspect.max(0.001),
            self.near,
            self.far,
        )
    }

    /// The combined view-projection matrix.
    #[must_use]
    pub fn view_projection(&self, aspect: f32) -> Mat4 {
        self.projection(aspect) * self.view()
    }

    /// The model matrix for one chunk: a translation by its floating-origin
    /// offset, scaled from sub-node cells to blocks.
    ///
    /// Mesh vertices are in sub-node units (`0..=48`), so the scale converts
    /// them to blocks before the translation places the chunk.
    #[must_use]
    pub fn chunk_model(&self, chunk: ChunkPos) -> Mat4 {
        let offset = self.position.chunk_offset(chunk);
        Mat4::from_translation(offset)
            * Mat4::from_scale(Vec3::splat(1.0 / SUBNODES_PER_AXIS as f32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The largest coordinate the world reaches: 120,000 blocks (charter
    /// rule 6), well past the ±50,000 the acceptance criterion names.
    const WORLD_EDGE: f64 = 120_000.0;

    #[test]
    fn a_world_position_round_trips_through_the_split() {
        for (x, y, z) in [
            (0.0, 0.0, 0.0),
            (1.5, 2.25, 3.75),
            (50_000.0, 64.0, -50_000.0),
            (-1.0, -1.0, -1.0),
            (WORLD_EDGE, WORLD_EDGE, -WORLD_EDGE),
        ] {
            let position = Position::from_world(x, y, z);
            let (rx, ry, rz) = position.to_world();
            // Within a thousandth of a block: `local` is f32, so some loss is
            // expected, but it must be far below a sub-node (0.333 blocks).
            assert!(
                (rx - x).abs() < 0.001 && (ry - y).abs() < 0.001 && (rz - z).abs() < 0.001,
                "({x}, {y}, {z}) round-tripped to ({rx}, {ry}, {rz})"
            );
        }
    }

    #[test]
    fn negative_coordinates_land_in_the_right_chunk() {
        // `as i32` truncates toward zero. Using it here puts everything west
        // and below the origin one chunk out, and the bug looks like a seam
        // that only exists on one side of the world.
        let position = Position::from_world(-1.0, -1.0, -1.0);
        assert_eq!(position.chunk, ChunkPos::new(-1, -1, -1));
        assert!(
            position.local.x > 14.9 && position.local.x < 15.1,
            "local was {}",
            position.local.x
        );

        assert_eq!(Position::from_world(-16.0, 0.0, 0.0).chunk.x, -1);
        assert_eq!(Position::from_world(-16.5, 0.0, 0.0).chunk.x, -2);
        assert_eq!(Position::from_world(15.9, 0.0, 0.0).chunk.x, 0);
        assert_eq!(Position::from_world(16.0, 0.0, 0.0).chunk.x, 1);
    }

    #[test]
    fn local_coordinates_stay_inside_the_chunk() {
        // The invariant the whole design rests on. If `local` could exceed a
        // chunk, its magnitude would grow without bound and the precision
        // argument would collapse.
        let mut position = Position::default();
        for _ in 0..10_000 {
            position.translate(Vec3::new(7.3, -3.1, 11.9));
            for axis in 0..3 {
                assert!(
                    position.local[axis] >= 0.0 && position.local[axis] < CHUNK_BLOCKS as f32,
                    "local[{axis}] drifted to {}",
                    position.local[axis]
                );
            }
        }
    }

    #[test]
    fn the_draw_offset_depends_on_the_view_distance_and_not_on_where_you_are() {
        // THE floating-origin property, and the [A]-assertable half of the
        // ±50,000 block acceptance criterion.
        //
        // Rendering at the origin and rendering at the edge of the world must
        // push identical numbers through the pipeline.
        let view_distance = 8;
        let max_expected = (view_distance as f32 + 1.0) * CHUNK_BLOCKS as f32;

        for centre in [0.0, 1_000.0, 50_000.0, -50_000.0, WORLD_EDGE, -WORLD_EDGE] {
            let camera = Position::from_world(centre, 64.0, centre);

            for dx in -view_distance..=view_distance {
                for dz in -view_distance..=view_distance {
                    let chunk =
                        ChunkPos::new(camera.chunk.x + dx, camera.chunk.y, camera.chunk.z + dz);
                    let offset = camera.chunk_offset(chunk);

                    assert!(
                        offset.length() <= max_expected * 1.5,
                        "at world {centre}, chunk offset {offset:?} has magnitude {} — the \
                         offset must be bounded by the view distance, not by the position",
                        offset.length()
                    );
                    assert!(
                        offset.is_finite(),
                        "at world {centre}, offset {offset:?} is not finite"
                    );
                }
            }
        }
    }

    #[test]
    fn the_draw_offset_keeps_sub_node_precision_at_the_edge_of_the_world() {
        // The symptom floating origin prevents is jitter: vertices snapping
        // between representable values as the camera moves. This measures the
        // representable step at the offsets actually produced.
        //
        // A sub-node is 1/3 of a block. Anything finer than a hundredth of that
        // is invisible.
        let sub_node = 1.0 / SUBNODES_PER_AXIS as f32;
        let tolerance = sub_node / 100.0;

        for centre in [0.0, 50_000.0, -50_000.0, WORLD_EDGE] {
            let camera = Position::from_world(centre, 64.0, centre);
            let chunk = ChunkPos::new(camera.chunk.x + 8, camera.chunk.y, camera.chunk.z + 8);
            let offset = camera.chunk_offset(chunk);

            // The gap to the next representable f32 at this magnitude.
            let step = {
                let value = offset.length().abs().max(1.0);
                let next = f32::from_bits(value.to_bits() + 1);
                next - value
            };

            assert!(
                step < tolerance,
                "at world {centre}, the offset magnitude {} has a representable step of {step}, \
                 which is coarser than a hundredth of a sub-node ({tolerance})",
                offset.length()
            );
        }
    }

    #[test]
    fn a_teleport_of_fifty_thousand_blocks_does_not_change_the_relative_geometry() {
        // The teleport case, stated as an equality rather than a bound: the
        // chunk under the camera, and its neighbours, must be at exactly the
        // same offsets before and after. That is what "no visible jitter"
        // means numerically.
        let near_origin = Position::from_world(8.0, 64.0, 8.0);
        let far_away = Position::from_world(50_000.0 + 8.0, 64.0, 50_000.0 + 8.0);

        for dx in -2..=2 {
            for dy in -2..=2 {
                for dz in -2..=2 {
                    let here = near_origin.chunk_offset(ChunkPos::new(
                        near_origin.chunk.x + dx,
                        near_origin.chunk.y + dy,
                        near_origin.chunk.z + dz,
                    ));
                    let there = far_away.chunk_offset(ChunkPos::new(
                        far_away.chunk.x + dx,
                        far_away.chunk.y + dy,
                        far_away.chunk.z + dz,
                    ));

                    assert!(
                        (here - there).length() < 1e-4,
                        "chunk ({dx}, {dy}, {dz}) sits at {here:?} near the origin but \
                         {there:?} at 50,000 blocks — the geometry moved"
                    );
                }
            }
        }
    }

    #[test]
    fn a_world_space_f32_would_have_failed_this() {
        // The counter-example, so the test above is visibly non-vacuous. This
        // is what the naive approach does at 50,000 blocks, and why charter
        // rule 7 exists.
        let coarse = 50_000.0_f32;
        let step = f32::from_bits(coarse.to_bits() + 1) - coarse;
        let sub_node = 1.0 / SUBNODES_PER_AXIS as f32;

        assert!(
            step > sub_node / 100.0,
            "the premise of floating origin is that f32 world coordinates are too coarse at \
             50,000 blocks; the step there is {step} against a sub-node of {sub_node}"
        );
    }

    #[test]
    fn the_camera_is_always_at_the_origin_of_view_space() {
        // Floating origin, stated as a property of the matrix: the view matrix
        // must not carry a translation, because geometry is moved to the
        // camera rather than the camera to the geometry.
        let camera = Camera {
            position: Position::from_world(50_000.0, 100.0, -50_000.0),
            ..Camera::default()
        };
        let view = camera.view();
        let translation = view.w_axis.truncate();

        assert!(
            translation.length() < 1e-6,
            "the view matrix carries a translation of {translation:?}; the camera must sit at \
             the origin of view space whatever its world position"
        );
    }

    #[test]
    fn pitch_is_clamped_away_from_vertical() {
        // Exactly vertical makes the basis degenerate and the camera spins for
        // a frame.
        let mut camera = Camera::default();
        camera.look(0.0, 100.0);
        assert!(camera.pitch < std::f32::consts::FRAC_PI_2);
        assert!(camera.forward().is_normalized());

        camera.look(0.0, -200.0);
        assert!(camera.pitch > -std::f32::consts::FRAC_PI_2);
        assert!(camera.forward().is_normalized());
        assert!(camera.right().length() > 0.5, "the basis must not collapse");
    }

    #[test]
    fn yaw_wraps_rather_than_growing() {
        // Left unbounded, yaw accumulates until f32 precision makes mouse
        // movement chunky — after a long session, not immediately, which is
        // the worst kind of bug to find.
        let mut camera = Camera::default();
        for _ in 0..1000 {
            camera.look(1.0, 0.0);
        }
        assert!(
            camera.yaw >= 0.0 && camera.yaw < std::f32::consts::TAU,
            "yaw grew to {}",
            camera.yaw
        );
    }

    #[test]
    fn moving_the_mouse_right_slides_the_scene_left() {
        // The human gate caught this and no test did: mouse-look was inverted
        // horizontally because `forward` swung toward +x as yaw grew, and +x is
        // west. Asserting on axes would just re-state whichever convention the
        // code happens to hold, so this asserts what the player actually sees —
        // a marker dead ahead must slide LEFT across the screen when the view
        // turns right, and it goes through the real view-projection to do it.
        let mut camera = Camera::default();
        let marker = camera.forward() * 10.0;

        let centred = clip_x(&camera, marker);
        assert!(
            centred.abs() < 1e-5,
            "the marker should start dead centre, not at {centred}"
        );

        camera.look(0.3, 0.0);
        let after_turning_right = clip_x(&camera, marker);
        assert!(
            after_turning_right < -0.1,
            "turning right left the marker at {after_turning_right}; negative is left of centre, \
             so a positive value here is the inverted mouse-look the gate reported"
        );

        camera.look(-0.6, 0.0);
        let after_turning_left = clip_x(&camera, marker);
        assert!(
            after_turning_left > 0.1,
            "turning left should have thrown the marker to the right, not to {after_turning_left}"
        );
    }

    #[test]
    fn turning_right_faces_east_and_east_is_negative_x() {
        // +y up and +z north makes +x = up × north = west, so the compass in
        // the HUD is only honest if a right turn from north points at −x.
        let mut camera = Camera::default();
        assert!((camera.forward() - Vec3::Z).length() < 1e-6, "north is +z");

        camera.look(std::f32::consts::FRAC_PI_2, 0.0);
        let facing = camera.forward();
        assert!(
            (facing - Vec3::NEG_X).length() < 1e-5,
            "a quarter turn right should face east at −x, not {facing:?}"
        );
    }

    /// Where a world offset lands across the screen: −1 is the left edge, +1
    /// the right. The camera sits at the origin, so an offset is a position.
    fn clip_x(camera: &Camera, offset: Vec3) -> f32 {
        let clip = camera.view_projection(1.0) * glam::Vec4::new(offset.x, offset.y, offset.z, 1.0);
        clip.x / clip.w
    }

    #[test]
    fn flying_forward_moves_along_the_view_direction() {
        let mut camera = Camera {
            yaw: 0.0,
            pitch: 0.0,
            ..Camera::default()
        };
        let before = camera.position.to_world();

        camera.fly(10.0, 0.0, 0.0);
        let after = camera.position.to_world();

        // Default yaw looks along +z.
        assert!(
            after.2 > before.2 + 9.0,
            "did not move forward: {before:?} -> {after:?}"
        );
        assert!(
            (after.1 - before.1).abs() < 0.001,
            "should not have changed height"
        );
    }

    #[test]
    fn the_chunk_model_scales_sub_nodes_to_blocks() {
        // Mesh vertices are in sub-node units 0..=48; a chunk spans 16 blocks.
        // Getting the scale wrong makes the world three times too big, which
        // looks like a meshing bug.
        let camera = Camera::default();
        let model = camera.chunk_model(camera.position.chunk);

        let corner = model.transform_point3(Vec3::new(48.0, 0.0, 0.0));
        let origin = model.transform_point3(Vec3::ZERO);
        assert!(
            ((corner - origin).x - 16.0).abs() < 0.001,
            "48 sub-nodes should span 16 blocks, got {}",
            (corner - origin).x
        );
    }
}
