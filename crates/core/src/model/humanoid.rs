// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! `engine:humanoid` — the one rig the engine ships.
//!
//! # Why this is Rust and not a committed `.glb`
//!
//! Because a binary blob in a repository is not source. Nobody can review it,
//! a diff on it says nothing, and the first time somebody needs the arm a
//! quarter-cell longer they need a modelling package and a export pipeline
//! that is not in this project. Written as code, the rig is a table of
//! measurements next to the physics constants it has to agree with — and it
//! does have to agree: a body that draws taller than [`crate::phys::Shape`]
//! collides at is a body that visibly clips into ceilings.
//!
//! [`super::build::to_glb`] turns it into a real `.glb` for the fuzz corpus and
//! for anyone who wants to open it, so nothing is lost by not committing one.
//!
//! # Why the engine ships a rig at all
//!
//! Charter rule 1 says content is mods, and this is the exception the task
//! names: players have to be drawn as *something* before any mod has loaded,
//! and a client that had to wait for a mod to tell it what a person looks like
//! would show nothing on a modless server. So the engine owns exactly one
//! humanoid, mods may use it, and everything else is theirs.
//!
//! Untextured, which is matte white. Skins are a later phase and deliberately
//! not started here.
//!
//! # Determinism does not reach here
//!
//! Charter rule 4's scope is worldgen, the tick and the hash gate. This is
//! presentation: `sin` and `cos` are used freely to build quaternions, because
//! two clients disagreeing about an elbow cannot make two worlds disagree.

use super::{Channel, Clip, Joint, Model, Pose, Property, Skin, Vertex};
use crate::ent::AnimTag;

/// How tall the rig stands, in cells.
///
/// [`crate::phys::PLAYER_HEIGHT`], because it is the same body: the box the
/// server collides and the mesh the client draws have to be the same size or
/// the drawing is a lie.
pub const HEIGHT: f32 = crate::phys::PLAYER_HEIGHT;

/// How wide it is across the shoulders, in cells. Matches the collider.
pub const WIDTH: f32 = crate::phys::PLAYER_WIDTH;

/// The clip a client plays for a server-sent state tag.
///
/// **The server never names a clip.** It says "walking" and this is where that
/// becomes an animation, which is the split that keeps skeletal animation out
/// of the simulation entirely. A tag with no clip falls back to `idle` rather
/// than freezing, so a mod registering its own tag against this rig gets
/// something standing still instead of something inert.
#[must_use]
pub fn clip_for(tag: AnimTag) -> &'static str {
    match tag {
        AnimTag::WALK => "walk",
        AnimTag::RUN => "run",
        AnimTag::SWING => "swing",
        AnimTag::SWIM => "swim",
        AnimTag::SNEAK => "sneak",
        _ => "idle",
    }
}

/// One bone: what it is called, where it sits at rest, and the box drawn for it.
struct Bone {
    name: &'static str,
    parent: Option<&'static str>,
    /// Position in model space at rest, in cells. Feet at `y = 0`.
    at: [f32; 3],
    /// The box this bone draws, as `(min, max)` offsets from `at`.
    ///
    /// `None` for a bone that only steers others — the hips, which every limb
    /// hangs from and which would otherwise draw a box inside the chest.
    box_: Option<([f32; 3], [f32; 3])>,
}

/// The skeleton, in the order it is written.
///
/// Parents before children, which [`super::ingest`] also guarantees for a
/// loaded model — a single forward pass builds world matrices either way.
///
/// Proportions are a blocky humanoid rather than a realistic one, which is the
/// right answer for a voxel world and also the honest one: every box here is
/// axis-aligned, so nothing in the mesh needs a modelling tool to change.
#[allow(
    clippy::too_many_lines,
    reason = "a table of measurements; splitting it would hide the proportions"
)]
fn bones() -> Vec<Bone> {
    let half = WIDTH / 2.0;
    let hip = HEIGHT * 0.45;
    let shoulder = HEIGHT * 0.76;
    let neck = HEIGHT * 0.80;

    vec![
        Bone {
            name: "hips",
            parent: None,
            at: [0.0, hip, 0.0],
            box_: None,
        },
        Bone {
            name: "chest",
            parent: Some("hips"),
            at: [0.0, hip, 0.0],
            box_: Some((
                [-half * 0.66, 0.0, -half * 0.34],
                [half * 0.66, shoulder - hip, half * 0.34],
            )),
        },
        Bone {
            name: "head",
            parent: Some("chest"),
            at: [0.0, neck, 0.0],
            box_: Some((
                [-half * 0.6, 0.0, -half * 0.6],
                [half * 0.6, HEIGHT - neck, half * 0.6],
            )),
        },
        // Arms hang from the shoulders, so the upper arm's box runs downwards.
        Bone {
            name: "arm.l",
            parent: Some("chest"),
            at: [half * 0.8, shoulder, 0.0],
            box_: Some((
                [-half * 0.18, -(shoulder - hip) * 0.5, -half * 0.18],
                [half * 0.18, 0.0, half * 0.18],
            )),
        },
        Bone {
            name: "hand.l",
            parent: Some("arm.l"),
            at: [half * 0.8, shoulder - (shoulder - hip) * 0.5, 0.0],
            box_: Some((
                [-half * 0.18, -(shoulder - hip) * 0.5, -half * 0.18],
                [half * 0.18, 0.0, half * 0.18],
            )),
        },
        Bone {
            name: "arm.r",
            parent: Some("chest"),
            at: [-half * 0.8, shoulder, 0.0],
            box_: Some((
                [-half * 0.18, -(shoulder - hip) * 0.5, -half * 0.18],
                [half * 0.18, 0.0, half * 0.18],
            )),
        },
        Bone {
            name: "hand.r",
            parent: Some("arm.r"),
            at: [-half * 0.8, shoulder - (shoulder - hip) * 0.5, 0.0],
            box_: Some((
                [-half * 0.18, -(shoulder - hip) * 0.5, -half * 0.18],
                [half * 0.18, 0.0, half * 0.18],
            )),
        },
        Bone {
            name: "leg.l",
            parent: Some("hips"),
            at: [half * 0.35, hip, 0.0],
            box_: Some((
                [-half * 0.28, -hip * 0.5, -half * 0.28],
                [half * 0.28, 0.0, half * 0.28],
            )),
        },
        Bone {
            name: "foot.l",
            parent: Some("leg.l"),
            at: [half * 0.35, hip * 0.5, 0.0],
            box_: Some((
                [-half * 0.28, -hip * 0.5, -half * 0.28],
                [half * 0.28, 0.0, half * 0.5],
            )),
        },
        Bone {
            name: "leg.r",
            parent: Some("hips"),
            at: [-half * 0.35, hip, 0.0],
            box_: Some((
                [-half * 0.28, -hip * 0.5, -half * 0.28],
                [half * 0.28, 0.0, half * 0.28],
            )),
        },
        Bone {
            name: "foot.r",
            parent: Some("leg.r"),
            at: [-half * 0.35, hip * 0.5, 0.0],
            box_: Some((
                [-half * 0.28, -hip * 0.5, -half * 0.28],
                [half * 0.28, 0.0, half * 0.5],
            )),
        },
    ]
}

/// The engine's humanoid, mesh, skeleton and clips.
#[must_use]
pub fn humanoid() -> Model {
    let bones = bones();
    let index_of = |name: &str| {
        bones
            .iter()
            .position(|bone| bone.name == name)
            .and_then(|index| u8::try_from(index).ok())
    };

    let mut skin = Skin::default();
    for bone in &bones {
        let parent = bone.parent.and_then(&index_of);
        // Local translation: where this bone sits relative to its parent, since
        // that is what a glTF node stores and what an animation composes with.
        let parent_at = bone
            .parent
            .and_then(|name| bones.iter().find(|other| other.name == name))
            .map_or([0.0; 3], |other| other.at);
        skin.joints.push(Joint {
            name: bone.name.to_owned(),
            parent,
            rest: Pose {
                translation: [
                    bone.at[0] - parent_at[0],
                    bone.at[1] - parent_at[1],
                    bone.at[2] - parent_at[2],
                ],
                ..Pose::default()
            },
            // Every rest rotation is the identity and every scale is one, so a
            // bone's world rest matrix is a pure translation — and its inverse
            // is the same translation negated. Written out rather than
            // computed, because a general matrix inverse here would be code
            // nothing else needs and one more place to be wrong.
            inverse_bind: translation(-bone.at[0], -bone.at[1], -bone.at[2]),
        });
    }

    let mut model = Model {
        skin,
        ..Model::default()
    };
    for (index, bone) in bones.iter().enumerate() {
        let Some((min, max)) = bone.box_ else {
            continue;
        };
        let joint = u8::try_from(index).unwrap_or(0);
        add_box(
            &mut model,
            [
                bone.at[0] + min[0],
                bone.at[1] + min[1],
                bone.at[2] + min[2],
            ],
            [
                bone.at[0] + max[0],
                bone.at[1] + max[1],
                bone.at[2] + max[2],
            ],
            joint,
        );
    }

    model.clips = clips(&index_of);
    model
}

/// A column-major translation matrix.
fn translation(x: f32, y: f32, z: f32) -> [f32; 16] {
    [
        1.0, 0.0, 0.0, 0.0, //
        0.0, 1.0, 0.0, 0.0, //
        0.0, 0.0, 1.0, 0.0, //
        x, y, z, 1.0,
    ]
}

/// Adds an axis-aligned box, rigidly weighted to one joint.
///
/// Twenty-four vertices rather than eight: a cube's corner has three different
/// normals, and sharing them would give every face the average of the three,
/// which lights a box like a sphere.
fn add_box(model: &mut Model, min: [f32; 3], max: [f32; 3], joint: u8) {
    const FACES: [([f32; 3], [usize; 4]); 6] = [
        ([0.0, 0.0, 1.0], [0, 1, 3, 2]),  // +z
        ([0.0, 0.0, -1.0], [5, 4, 6, 7]), // -z
        ([1.0, 0.0, 0.0], [1, 5, 7, 3]),  // +x
        ([-1.0, 0.0, 0.0], [4, 0, 2, 6]), // -x
        ([0.0, 1.0, 0.0], [2, 3, 7, 6]),  // +y
        ([0.0, -1.0, 0.0], [4, 5, 1, 0]), // -y
    ];

    // Corner `i` takes its x from bit 0, y from bit 1 and z from bit 2.
    let corner = |index: usize| {
        [
            if index & 1 == 0 { min[0] } else { max[0] },
            if index & 2 == 0 { min[1] } else { max[1] },
            if index & 4 == 0 { max[2] } else { min[2] },
        ]
    };

    for (normal, quad) in FACES {
        let base = u32::try_from(model.vertices.len()).unwrap_or(0);
        for (slot, index) in quad.into_iter().enumerate() {
            model.vertices.push(Vertex {
                position: corner(index),
                normal,
                // A single flat UV: the rig is untextured white, and a texture
                // layout is a decision for whoever adds skins.
                uv: [f32::from(u8::try_from(slot % 2).unwrap_or(0)), 0.0],
                joints: [joint, 0, 0, 0],
                weights: [1.0, 0.0, 0.0, 0.0],
            });
        }
        model
            .indices
            .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
}

/// A quaternion for a rotation about the X axis, `[x, y, z, w]`.
///
/// `sin` and `cos` are on charter rule 4's banned list, which is scoped to
/// `crates/core` because `clippy.toml` cannot be scoped per module. Rule 4's
/// own **Scope** section exempts presentation, and this is presentation living
/// in `core` for the reason `docs/float-determinism.md` gives under
/// "Presentation code that lives in `crates/core`": the rig has to be testable
/// and fuzzable without a GPU.
#[allow(
    clippy::disallowed_methods,
    reason = "presentation; float-determinism.md Scope"
)]
fn pitch(radians: f32) -> [f32; 4] {
    let half = radians / 2.0;
    [half.sin(), 0.0, 0.0, half.cos()]
}

/// A quaternion for a rotation about the Z axis. See [`pitch`] on the lint.
#[allow(
    clippy::disallowed_methods,
    reason = "presentation; float-determinism.md Scope"
)]
fn roll(radians: f32) -> [f32; 4] {
    let half = radians / 2.0;
    [0.0, 0.0, half.sin(), half.cos()]
}

/// The five clips the task names, plus the sneak the engine's own tag needs.
fn clips(index_of: &impl Fn(&str) -> Option<u8>) -> Vec<Clip> {
    let joint = |name: &str| index_of(name).unwrap_or(0);
    vec![
        idle(&joint),
        gait("walk", 1.0, 0.55, 0.45, 0.0, &joint),
        // Faster, longer strides, and leaning into it — which is the difference
        // between a run and a fast walk at a glance.
        gait("run", 0.6, 0.95, 0.8, 0.22, &joint),
        swing_clip(&joint),
        swim(&joint),
        sneak(&joint),
    ]
}

/// A limb swing, as five keyframes over one period: forward, neutral, back,
/// neutral, and round again.
///
/// `phase` flips it, which is what makes an arm and the leg under it move
/// opposite each other.
fn swing(joint: u8, period: f32, reach: f32, phase: bool) -> Channel {
    let sign = if phase { -1.0 } else { 1.0 };
    Channel {
        joint,
        property: Property::Rotation,
        times: vec![0.0, period * 0.25, period * 0.5, period * 0.75, period],
        values: [
            pitch(reach * sign),
            pitch(0.0),
            pitch(-reach * sign),
            pitch(0.0),
            pitch(reach * sign),
        ]
        .into_iter()
        .flatten()
        .collect(),
    }
}

/// A channel holding one rotation for a whole clip.
fn held(joint: u8, rotation: [f32; 4], duration: f32) -> Channel {
    Channel {
        joint,
        property: Property::Rotation,
        times: vec![0.0, duration],
        values: [rotation, rotation].into_iter().flatten().collect(),
    }
}

/// **Idle is not nothing.** A body that holds one pose exactly reads as a
/// statue, and the eye notices the stillness before it notices anything else. A
/// slow breath on the chest is the cheapest fix there is.
fn idle(joint: &impl Fn(&str) -> u8) -> Clip {
    Clip {
        name: "idle".to_owned(),
        duration: 3.0,
        channels: vec![Channel {
            joint: joint("chest"),
            property: Property::Rotation,
            times: vec![0.0, 1.5, 3.0],
            values: [pitch(0.0), pitch(0.03), pitch(0.0)]
                .into_iter()
                .flatten()
                .collect(),
        }],
    }
}

/// A walking or running cycle: legs and arms in opposite phase, and an optional
/// forward lean.
fn gait(
    name: &str,
    period: f32,
    legs: f32,
    arms: f32,
    lean: f32,
    joint: &impl Fn(&str) -> u8,
) -> Clip {
    let mut channels = vec![
        swing(joint("leg.l"), period, legs, false),
        swing(joint("leg.r"), period, legs, true),
        swing(joint("arm.l"), period, arms, true),
        swing(joint("arm.r"), period, arms, false),
    ];
    if lean.abs() > f32::EPSILON {
        channels.push(held(joint("chest"), pitch(lean), period));
    }
    Clip {
        name: name.to_owned(),
        duration: period,
        channels,
    }
}

/// A swing is a one-shot rather than a cycle: it ends where it started, so a
/// client can play it once and fall back to whatever the body was doing.
fn swing_clip(joint: &impl Fn(&str) -> u8) -> Clip {
    Clip {
        name: "swing".to_owned(),
        duration: 0.4,
        channels: vec![Channel {
            joint: joint("arm.r"),
            property: Property::Rotation,
            times: vec![0.0, 0.12, 0.28, 0.4],
            values: [pitch(-2.2), pitch(-2.6), pitch(0.4), pitch(0.0)]
                .into_iter()
                .flatten()
                .collect(),
        }],
    }
}

fn swim(joint: &impl Fn(&str) -> u8) -> Clip {
    Clip {
        name: "swim".to_owned(),
        duration: 1.6,
        channels: vec![
            // Pitched forward, because a body swimming upright is a body
            // standing in water.
            held(joint("hips"), pitch(1.2), 1.6),
            swing(joint("arm.l"), 1.6, 1.1, false),
            swing(joint("arm.r"), 1.6, 1.1, true),
            swing(joint("leg.l"), 1.6, 0.3, true),
            swing(joint("leg.r"), 1.6, 0.3, false),
        ],
    }
}

/// Sneak is a crouch, not a gait: the body drops and the legs shift, and the
/// walk underneath it keeps playing on a client that blends.
fn sneak(joint: &impl Fn(&str) -> u8) -> Clip {
    Clip {
        name: "sneak".to_owned(),
        duration: 1.0,
        channels: vec![
            Channel {
                joint: joint("hips"),
                property: Property::Translation,
                times: vec![0.0, 1.0],
                values: vec![0.0, -HEIGHT * 0.12, 0.0, 0.0, -HEIGHT * 0.12, 0.0],
            },
            held(joint("chest"), pitch(0.35), 1.0),
            Channel {
                joint: joint("leg.l"),
                property: Property::Rotation,
                times: vec![0.0, 0.5, 1.0],
                values: [roll(0.12), roll(-0.12), roll(0.12)]
                    .into_iter()
                    .flatten()
                    .collect(),
            },
            Channel {
                joint: joint("leg.r"),
                property: Property::Rotation,
                times: vec![0.0, 0.5, 1.0],
                values: [roll(-0.12), roll(0.12), roll(-0.12)]
                    .into_iter()
                    .flatten()
                    .collect(),
            },
        ],
    }
}
