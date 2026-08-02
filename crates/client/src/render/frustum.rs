// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Frustum culling for chunk meshes.
//!
//! # Why this is worth doing before anything else is optimised
//!
//! At view distance 8 the interest volume is roughly 17×17×9 — about 2,600
//! chunks — and a camera with a 70° field of view can see under a third of
//! them. Drawing the rest costs a draw call, a vertex buffer bind, and a full
//! vertex shader pass each, all to produce nothing.
//!
//! # Extracting planes from the view-projection matrix
//!
//! The six frustum planes fall out of the combined matrix directly: a point is
//! inside the left plane when `clip.x > -clip.w`, which rearranges to
//! `row3 + row0 ≥ 0` applied to the world-space point. Doing it this way means
//! the culler cannot disagree with the projection — there is no second copy of
//! the field of view, the aspect ratio, or the near and far planes to get out
//! of step. (Gribb & Hartmann, "Fast Extraction of Viewing Frustum Planes".)
//!
//! # Charter rule 4 does not reach here
//!
//! This is presentation. A chunk wrongly culled is a chunk that is not drawn
//! for one frame; nothing in the simulation can observe it.

use glam::{Mat4, Vec3, Vec4};

/// The six planes of a view frustum, each as `ax + by + cz + d = 0` with the
/// normal pointing **inwards**.
#[derive(Debug, Clone, Copy)]
pub struct Frustum {
    planes: [Vec4; 6],
}

impl Frustum {
    /// Extracts the planes from a view-projection matrix.
    #[must_use]
    pub fn from_view_projection(view_projection: Mat4) -> Self {
        // `glam` is column-major, so a "row" of the matrix is one component
        // taken across the four columns. Reading columns instead is the classic
        // way to get a culler that is transposed — and a transposed frustum
        // still culls *something*, so it looks like it works until the camera
        // turns.
        let matrix = view_projection.to_cols_array_2d();
        let row = |index: usize| {
            Vec4::new(
                matrix[0][index],
                matrix[1][index],
                matrix[2][index],
                matrix[3][index],
            )
        };
        let (x, y, z, w) = (row(0), row(1), row(2), row(3));

        // Near is `z ≥ 0` rather than `z ≥ -w`: the projection here is the
        // zero-to-one depth convention (`proj::directx`), not OpenGL's
        // minus-one-to-one. Using the wrong one culls everything closer than
        // halfway to the far plane, which looks like the world is missing.
        Self {
            planes: [
                normalise(w + x), // left
                normalise(w - x), // right
                normalise(w + y), // bottom
                normalise(w - y), // top
                normalise(z),     // near
                normalise(w - z), // far
            ],
        }
    }

    /// Whether an axis-aligned box is at least partly inside.
    ///
    /// Conservative: a box the test keeps but that is actually outside costs
    /// one wasted draw call, while a box it wrongly rejects is a hole in the
    /// world. The asymmetry is the whole reason this uses the "positive vertex"
    /// test rather than anything cleverer.
    #[must_use]
    pub fn intersects_box(&self, min: Vec3, max: Vec3) -> bool {
        for plane in &self.planes {
            let normal = plane.truncate();
            // The corner furthest along the plane's normal. If even that corner
            // is behind the plane, every corner is, and the box is out.
            let positive = Vec3::new(
                if normal.x >= 0.0 { max.x } else { min.x },
                if normal.y >= 0.0 { max.y } else { min.y },
                if normal.z >= 0.0 { max.z } else { min.z },
            );
            if normal.dot(positive) + plane.w < 0.0 {
                return false;
            }
        }
        true
    }

    /// Whether a chunk is visible, given its camera-relative offset in blocks.
    ///
    /// The offset is what [`crate::camera::Position::chunk_offset`] produces:
    /// bounded by the view distance rather than by where in the world the
    /// camera is, which is what keeps this precise at the edge of the world.
    #[must_use]
    pub fn contains_chunk(&self, offset: Vec3) -> bool {
        let span = tiamot_core::CHUNK_BLOCKS as f32;
        self.intersects_box(offset, offset + Vec3::splat(span))
    }
}

/// Scales a plane so its normal is unit length.
///
/// Not needed for a pure inside/outside test, but it makes the `w` term an
/// actual distance, which is what any later use — LOD selection, fade bands —
/// would want. A zero-length normal is left alone rather than producing NaN.
fn normalise(plane: Vec4) -> Vec4 {
    let length = plane.truncate().length();
    if length > 0.0 { plane / length } else { plane }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera::{Camera, Position};

    /// A camera looking along +z from the origin, and its frustum.
    fn looking_forward() -> Frustum {
        let camera = Camera {
            position: Position::from_world(0.0, 0.0, 0.0),
            yaw: 0.0,
            pitch: 0.0,
            ..Camera::default()
        };
        Frustum::from_view_projection(camera.view_projection(16.0 / 9.0))
    }

    #[test]
    fn what_is_straight_ahead_is_visible() {
        let frustum = looking_forward();
        assert!(frustum.intersects_box(Vec3::new(-1.0, -1.0, 10.0), Vec3::new(1.0, 1.0, 12.0)));
    }

    #[test]
    fn what_is_behind_the_camera_is_not() {
        // The near plane. Getting the depth convention wrong here — using
        // OpenGL's `w + z` against a zero-to-one projection — culls everything
        // nearer than halfway to the far plane instead, and the world looks
        // like it has a hole around the player.
        let frustum = looking_forward();
        assert!(!frustum.intersects_box(Vec3::new(-1.0, -1.0, -20.0), Vec3::new(1.0, 1.0, -10.0)));
    }

    #[test]
    fn what_is_far_to_the_side_is_not() {
        let frustum = looking_forward();
        assert!(!frustum.intersects_box(Vec3::new(500.0, -1.0, 10.0), Vec3::new(510.0, 1.0, 12.0)));
        assert!(
            !frustum.intersects_box(Vec3::new(-510.0, -1.0, 10.0), Vec3::new(-500.0, 1.0, 12.0))
        );
    }

    #[test]
    fn a_box_straddling_the_edge_is_kept() {
        // Conservative on purpose: a wrongly kept box costs a draw call, a
        // wrongly rejected one is a hole in the world.
        let frustum = looking_forward();
        assert!(frustum.intersects_box(Vec3::new(-1000.0, -1.0, 10.0), Vec3::new(0.0, 1.0, 12.0)));
    }

    #[test]
    fn beyond_the_far_plane_is_not_visible() {
        let frustum = looking_forward();
        let far = Camera::default().far;
        assert!(!frustum.intersects_box(
            Vec3::new(-1.0, -1.0, far + 100.0),
            Vec3::new(1.0, 1.0, far + 200.0)
        ));
    }

    #[test]
    fn turning_the_camera_turns_the_frustum() {
        // A transposed extraction still culls something, so it looks like it
        // works until the camera turns. This is the test that would catch it.
        let ahead = Vec3::new(0.0, 0.0, 40.0);
        let behind = Vec3::new(0.0, 0.0, -40.0);
        let size = Vec3::splat(4.0);

        let forward = looking_forward();
        assert!(forward.contains_chunk(ahead));
        assert!(!forward.intersects_box(behind, behind + size));

        let turned = Camera {
            position: Position::from_world(0.0, 0.0, 0.0),
            yaw: std::f32::consts::PI,
            ..Camera::default()
        };
        let turned = Frustum::from_view_projection(turned.view_projection(16.0 / 9.0));
        assert!(turned.intersects_box(behind, behind + size));
        assert!(!turned.intersects_box(ahead, ahead + size));
    }

    #[test]
    fn a_chunk_the_camera_is_standing_in_is_always_visible() {
        // The case a culler must never get wrong. The camera sits inside this
        // box, so no plane can have every corner behind it — and a culler that
        // dropped it would blank the ground under the player's feet.
        let frustum = looking_forward();
        assert!(frustum.contains_chunk(Vec3::new(-8.0, -8.0, -8.0)));
    }

    #[test]
    fn culling_actually_rejects_most_of_the_interest_volume() {
        // The reason this module exists, as a number rather than a claim. At
        // view distance 8 a 70-degree camera should see well under half of
        // what is loaded; if this ever passes trivially, the culler has
        // stopped culling.
        let frustum = looking_forward();
        let camera = Position::from_world(0.0, 0.0, 0.0);
        let (mut total, mut visible) = (0, 0);

        for x in -8..=8 {
            for y in -4..=4 {
                for z in -8..=8 {
                    total += 1;
                    let offset = camera.chunk_offset(tiamot_core::ChunkPos::new(x, y, z));
                    if frustum.contains_chunk(offset) {
                        visible += 1;
                    }
                }
            }
        }

        assert!(
            visible * 2 < total,
            "the frustum kept {visible} of {total} chunks; a culler that keeps most of the \
             volume is not doing anything"
        );
        assert!(visible > 0, "and it must keep the ones in front of you");
    }
}
