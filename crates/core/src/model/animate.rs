// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Turning a clip and a time into the matrices a skinning shader wants.
//!
//! # Why this is in `core` when only the client draws
//!
//! Because it is arithmetic over data, with no GPU in it, and putting it here
//! means it can be tested without an adapter — the same argument that put the
//! parser here. The client uploads what this returns; the server never calls it.
//!
//! # Charter rule 4 does not reach this
//!
//! Its scope is worldgen, the simulation tick and the hash gate, and animation
//! is none of them. `sin`, `cos` and `sqrt` are used freely: two clients
//! disagreeing about an elbow by a float's last bit cannot make two worlds
//! disagree about anything, and the alternative — a lookup table for every
//! joint rotation — would cost precision in the one place nobody can measure it.
//!
//! # What a "skinning matrix" is here
//!
//! For each joint: its animated local transform, composed with every parent's,
//! then multiplied by its inverse bind matrix. A vertex in model space
//! multiplied by that lands where the animation puts it. The result is
//! column-major, which is what glTF stores and what WGSL expects.

use super::{Channel, Clip, Model, Pose, Property, Skin};

/// A column-major 4×4, as the GPU takes it.
pub type Matrix = [f32; 16];

/// The identity.
pub const IDENTITY: Matrix = [
    1.0, 0.0, 0.0, 0.0, //
    0.0, 1.0, 0.0, 0.0, //
    0.0, 0.0, 1.0, 0.0, //
    0.0, 0.0, 0.0, 1.0,
];

/// One matrix per joint, for a clip at a time.
///
/// `time` is in seconds and **wraps**: a clip is a loop, and a caller playing a
/// one-shot stops asking rather than expecting this to clamp. A clip with no
/// length, or a model with no skeleton, gives back the rest pose — which is a
/// figure standing still rather than a figure collapsed into the origin.
#[must_use]
pub fn skinning_matrices(model: &Model, clip: Option<&Clip>, time: f32) -> Vec<Matrix> {
    let joints = &model.skin.joints;
    if joints.is_empty() {
        return Vec::new();
    }

    // Start from the rest pose and let the clip override what it drives. A clip
    // that animates one arm must not reset the other one to the origin, which
    // is what building from zero would do.
    let mut local: Vec<Pose> = joints.iter().map(|joint| joint.rest).collect();

    if let Some(clip) = clip.filter(|clip| clip.duration > 0.0) {
        // `rem_euclid` rather than a modulo: a negative time from a client that
        // let its clock run backwards would otherwise index before the start.
        let at = time.rem_euclid(clip.duration);
        for channel in &clip.channels {
            let Some(pose) = local.get_mut(usize::from(channel.joint)) else {
                continue;
            };
            apply(channel, at, pose);
        }
    }

    // **One forward pass.** `ingest` and `humanoid` both guarantee parents come
    // before children, so a joint's parent is always already composed by the
    // time it is read — no recursion, no visited set, and no way for a cycle to
    // reach here.
    let mut world: Vec<Matrix> = Vec::with_capacity(joints.len());
    for (index, joint) in joints.iter().enumerate() {
        let own = matrix_of(&local[index]);
        let composed = match joint.parent {
            None => own,
            Some(parent) => multiply(&world[usize::from(parent)], &own),
        };
        world.push(composed);
    }

    world
        .iter()
        .zip(joints)
        .map(|(matrix, joint)| multiply(matrix, &joint.inverse_bind))
        .collect()
}

/// The rest pose, for a model with no clip to play.
#[must_use]
pub fn rest_matrices(skin: &Skin) -> Vec<Matrix> {
    let mut world: Vec<Matrix> = Vec::with_capacity(skin.joints.len());
    for joint in &skin.joints {
        let own = matrix_of(&joint.rest);
        let composed = match joint.parent {
            None => own,
            Some(parent) => multiply(&world[usize::from(parent)], &own),
        };
        world.push(composed);
    }
    world
        .iter()
        .zip(&skin.joints)
        .map(|(matrix, joint)| multiply(matrix, &joint.inverse_bind))
        .collect()
}

/// Writes one channel's value at `at` into a pose.
fn apply(channel: &Channel, at: f32, pose: &mut Pose) {
    let stride = channel.property.stride();
    let frames = channel.times.len();
    if frames == 0 || channel.values.len() < frames * stride {
        return;
    }

    // The pair of keyframes `at` falls between, and how far between. Linear
    // search because a clip has a handful of keyframes and a binary search over
    // five elements is slower than looking at all five.
    let mut index = frames - 1;
    for (slot, time) in channel.times.iter().enumerate() {
        if *time > at {
            index = slot.saturating_sub(1);
            break;
        }
    }
    let next = (index + 1).min(frames - 1);
    let span = channel.times[next] - channel.times[index];
    let fraction = if span > f32::EPSILON {
        ((at - channel.times[index]) / span).clamp(0.0, 1.0)
    } else {
        0.0
    };

    let a = &channel.values[index * stride..index * stride + stride];
    let b = &channel.values[next * stride..next * stride + stride];

    match channel.property {
        Property::Translation => {
            for axis in 0..3 {
                pose.translation[axis] = a[axis] + (b[axis] - a[axis]) * fraction;
            }
        }
        Property::Scale => {
            for axis in 0..3 {
                pose.scale[axis] = a[axis] + (b[axis] - a[axis]) * fraction;
            }
        }
        Property::Rotation => {
            pose.rotation = nlerp([a[0], a[1], a[2], a[3]], [b[0], b[1], b[2], b[3]], fraction);
        }
    }
}

/// Normalised linear interpolation between two quaternions.
///
/// **Not slerp**, and the difference is not worth what it costs here: over the
/// small angles between two adjacent keyframes of a walk cycle the two are
/// visually identical, and nlerp is four multiplies and a square root against
/// slerp's `acos`, `sin` and two divisions. The one thing it must do is take
/// the shorter way round, which is what the sign flip below is for — without it
/// a limb passing through the far side of a rotation snaps the wrong way.
fn nlerp(from: [f32; 4], to: [f32; 4], fraction: f32) -> [f32; 4] {
    let dot: f32 = from.iter().zip(to).map(|(a, b)| a * b).sum();
    let sign = if dot < 0.0 { -1.0 } else { 1.0 };

    let mut out = [0.0f32; 4];
    for axis in 0..4 {
        out[axis] = from[axis] + (to[axis] * sign - from[axis]) * fraction;
    }
    let length = (out.iter().map(|value| value * value).sum::<f32>()).sqrt();
    if length > f32::EPSILON {
        for value in &mut out {
            *value /= length;
        }
    } else {
        out = [0.0, 0.0, 0.0, 1.0];
    }
    out
}

/// A pose as a column-major matrix: scale, then rotate, then translate.
fn matrix_of(pose: &Pose) -> Matrix {
    let [x, y, z, w] = pose.rotation;
    let [sx, sy, sz] = pose.scale;

    // The standard quaternion-to-matrix, with each column scaled as it is
    // written rather than by a second multiply.
    let (xx, yy, zz) = (x * x, y * y, z * z);
    let (xy, xz, yz) = (x * y, x * z, y * z);
    let (wx, wy, wz) = (w * x, w * y, w * z);

    [
        (1.0 - 2.0 * (yy + zz)) * sx,
        (2.0 * (xy + wz)) * sx,
        (2.0 * (xz - wy)) * sx,
        0.0,
        (2.0 * (xy - wz)) * sy,
        (1.0 - 2.0 * (xx + zz)) * sy,
        (2.0 * (yz + wx)) * sy,
        0.0,
        (2.0 * (xz + wy)) * sz,
        (2.0 * (yz - wx)) * sz,
        (1.0 - 2.0 * (xx + yy)) * sz,
        0.0,
        pose.translation[0],
        pose.translation[1],
        pose.translation[2],
        1.0,
    ]
}

/// Column-major matrix product, `a` applied after `b`.
fn multiply(a: &Matrix, b: &Matrix) -> Matrix {
    let mut out = [0.0f32; 16];
    for column in 0..4 {
        for row in 0..4 {
            let mut sum = 0.0;
            for step in 0..4 {
                sum += a[step * 4 + row] * b[column * 4 + step];
            }
            out[column * 4 + row] = sum;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::humanoid;

    /// Where a joint ends up in model space, from its skinning matrix and its
    /// inverse bind: the two cancel at rest, so the origin of the joint's own
    /// space is the fourth column of the composed world matrix.
    fn moved(matrix: &Matrix) -> [f32; 3] {
        [matrix[12], matrix[13], matrix[14]]
    }

    #[test]
    fn the_rest_pose_moves_nothing() {
        // A rig with no clip playing has to draw exactly as it was modelled.
        // Every skinning matrix is the world rest matrix times its own inverse,
        // which is the identity — and if it is not, the model folds up the
        // moment it appears.
        let model = humanoid();
        for matrix in rest_matrices(&model.skin) {
            for (index, (got, want)) in matrix.iter().zip(IDENTITY).enumerate() {
                assert!(
                    (got - want).abs() < 1e-4,
                    "the rest pose is not the identity at {index}: {got} vs {want}"
                );
            }
        }
    }

    #[test]
    fn a_clip_at_time_zero_is_its_first_keyframe() {
        let model = humanoid();
        let walk = model.clip("walk").expect("walk");
        let start = skinning_matrices(&model, Some(walk), 0.0);
        let looped = skinning_matrices(&model, Some(walk), walk.duration);
        for (a, b) in start.iter().zip(&looped) {
            for (x, y) in a.iter().zip(b.iter()) {
                assert!(
                    (x - y).abs() < 1e-3,
                    "the clip does not loop: {x} at the start, {y} at the end"
                );
            }
        }
    }

    #[test]
    fn a_walk_actually_moves_the_legs() {
        // The weakest possible failure of an animation system is one that
        // returns the rest pose whatever it is asked. A leg a quarter of the
        // way through a walk cycle must not be where it started.
        let model = humanoid();
        let walk = model.clip("walk").expect("walk");
        let leg = model
            .skin
            .index_of("leg.l")
            .expect("the rig has a left leg");

        let rest = rest_matrices(&model.skin);
        // A TENTH of the way, not a quarter. The walk's keyframes are at
        // quarters and the ones at 0.25 and 0.75 are the neutral pose by
        // construction — a test sampling exactly there compares the rest pose
        // with the rest pose and fails for the one reason that is not a bug.
        let mid = skinning_matrices(&model, Some(walk), walk.duration * 0.1);
        let difference: f32 = rest[usize::from(leg)]
            .iter()
            .zip(&mid[usize::from(leg)])
            .map(|(a, b)| (a - b).abs())
            .sum();
        assert!(
            difference > 0.1,
            "the leg did not move a tenth of the way through a walk"
        );
    }

    #[test]
    fn a_child_follows_its_parent() {
        // The whole point of a skeleton. Rotating the chest has to carry the
        // head with it, and a hierarchy composed in the wrong order looks
        // exactly like one composed in no order at all — right until something
        // bends.
        let model = humanoid();
        let swim = model.clip("swim").expect("swim");
        let head = model.skin.index_of("head").expect("the rig has a head");

        let rest = rest_matrices(&model.skin);
        let posed = skinning_matrices(&model, Some(swim), 0.5);
        // `swim` pitches the hips, and the head hangs off the chest off the
        // hips — so a head that has not moved means the composition stopped
        // somewhere between.
        let moved_by: f32 = moved(&rest[usize::from(head)])
            .iter()
            .zip(moved(&posed[usize::from(head)]))
            .map(|(a, b)| (a - b).abs())
            .sum();
        assert!(
            moved_by > 0.01,
            "the head did not follow the hips, so the hierarchy is not composed"
        );
    }

    #[test]
    fn a_time_beyond_the_clip_wraps_rather_than_freezing() {
        let model = humanoid();
        let walk = model.clip("walk").expect("walk");
        let early = skinning_matrices(&model, Some(walk), 0.3);
        let later = skinning_matrices(&model, Some(walk), 0.3 + walk.duration * 4.0);
        for (a, b) in early.iter().zip(&later) {
            for (x, y) in a.iter().zip(b.iter()) {
                assert!((x - y).abs() < 1e-3, "the clip did not wrap");
            }
        }
    }

    #[test]
    fn a_negative_time_does_not_index_before_the_start() {
        // A client whose clock went backwards — a suspend, an NTP step — must
        // not index a negative keyframe. `rem_euclid` is what makes that true,
        // and a plain `%` would not be.
        let model = humanoid();
        let walk = model.clip("walk").expect("walk");
        let matrices = skinning_matrices(&model, Some(walk), -0.7);
        assert_eq!(matrices.len(), model.skin.joints.len());
        for matrix in &matrices {
            for value in matrix {
                assert!(value.is_finite(), "a negative time produced {value}");
            }
        }
    }

    #[test]
    fn a_model_with_no_skeleton_has_no_matrices() {
        let matrices = skinning_matrices(&crate::model::Model::default(), None, 0.0);
        assert!(matrices.is_empty());
    }

    #[test]
    #[allow(
        clippy::disallowed_methods,
        reason = "presentation; float-determinism.md Scope"
    )]
    fn a_rotation_takes_the_shorter_way_round() {
        // The sign flip in `nlerp`. Without it, two keyframes on opposite sides
        // of a rotation interpolate the long way and a limb snaps backwards
        // through the body.
        let quarter = std::f32::consts::FRAC_PI_4;
        let a = [quarter.sin(), 0.0, 0.0, quarter.cos()];
        let b = [-a[0], -a[1], -a[2], -a[3]];
        let half = nlerp(a, b, 0.5);
        // The two describe the SAME rotation, so every point between them
        // should be that rotation too — not a tumble through the origin.
        for (got, want) in half.iter().zip(a) {
            assert!(
                (got - want).abs() < 1e-4,
                "nlerp went the long way: {got} vs {want}"
            );
        }
    }
}
