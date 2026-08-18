// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Skinned models: what an entity is drawn as.
//!
//! # This lives in `core` and draws nothing
//!
//! Charter rule 3 keeps `wgpu` out of this crate, and none of it is here.
//! What is here is the *data*: a mesh with joint weights, a skeleton, and
//! clips. The client turns that into buffers and matrices; the server never
//! looks at it at all, because animation is presentation and the simulation has
//! no opinion about where an elbow is.
//!
//! Putting the parser here rather than in the client is what makes it testable
//! and fuzzable without a GPU — and charter rule 14 asks for a fuzz target in
//! the same task as the parser, which would be awkward to point at a crate that
//! needs an adapter to build a test for.
//!
//! # Everything in here arrived from somebody else's server
//!
//! Charter rule 14, and glTF is the largest hostile-input surface in the
//! project: a container format with an embedded JSON document, a binary blob,
//! and indices from one into the other. See [`ingest`] for the rules that
//! follow from that. The short version is that [`Limits`] is checked **before**
//! anything is allocated, external URIs are refused outright, and every
//! accessor is bounds-checked against the buffer it claims to read.
//!
//! # Determinism does not reach here
//!
//! Charter rule 4's scope is explicit: worldgen, the simulation tick, and the
//! CI hash gate. Animation is none of those. A joint matrix is built with
//! `sin` and `cos` and that is fine, because two clients disagreeing about
//! where an arm is cannot make two worlds disagree about anything.

pub mod animate;
pub mod build;
pub mod humanoid;
pub mod ingest;

pub use animate::{Matrix, rest_matrices, skinning_matrices};
pub use humanoid::{clip_for, humanoid};
pub use ingest::{ModelError, load, load_isolated};

/// One vertex of a skinned mesh.
///
/// Positions are **floats in model space**, which is the difference between
/// this and everything else the client draws. Voxel geometry snaps to a
/// sub-node cell and packs into a `u32`; a humanoid is 5.4 cells tall and
/// 1.8 wide, so a cell-quantised head would be one cell — see the entity
/// renderer, which drew boxes for exactly this reason until there was a rig.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vertex {
    /// Position in model space, in cells.
    pub position: [f32; 3],
    /// Unit normal.
    pub normal: [f32; 3],
    /// Texture coordinate.
    pub uv: [f32; 2],
    /// Which joints move it. Indices into [`Skin::joints`].
    ///
    /// **Four, and the number is a contract rather than a convenience.** The
    /// task names four weights, GPU skinning reads them as one `vec4`, and a
    /// fifth influence would double the per-vertex cost of every model in the
    /// game to serve a rig nobody here ships.
    pub joints: [u8; 4],
    /// How much each of those joints moves it. Sums to 1.
    pub weights: [f32; 4],
}

/// A joint in a skeleton.
#[derive(Debug, Clone, PartialEq)]
pub struct Joint {
    /// What it is called in the file, for a mod naming an attachment point.
    pub name: String,
    /// The joint above it, or `None` for a root.
    ///
    /// An index into [`Skin::joints`], and **always smaller than this joint's
    /// own index** — [`ingest`] sorts the skeleton so a single forward pass can
    /// build world matrices without a recursion or a visited set.
    pub parent: Option<u8>,
    /// Rest pose, relative to the parent: translation, rotation, scale.
    pub rest: Pose,
    /// Column-major matrix taking a vertex from model space into this joint's
    /// space, as glTF stores it.
    pub inverse_bind: [f32; 16],
}

/// A joint's local transform.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pose {
    /// Offset from the parent, in cells.
    pub translation: [f32; 3],
    /// Rotation as a quaternion, `[x, y, z, w]`.
    pub rotation: [f32; 4],
    /// Scale per axis.
    pub scale: [f32; 3],
}

impl Default for Pose {
    fn default() -> Self {
        Self {
            translation: [0.0; 3],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
        }
    }
}

/// A skeleton.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Skin {
    /// Joints, parents before children.
    pub joints: Vec<Joint>,
}

impl Skin {
    /// The index of a joint by name, for an attachment point.
    #[must_use]
    pub fn index_of(&self, name: &str) -> Option<u8> {
        self.joints
            .iter()
            .position(|joint| joint.name == name)
            .and_then(|index| u8::try_from(index).ok())
    }
}

/// Which part of a joint's transform a channel drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Property {
    /// Translation, three floats per keyframe.
    Translation,
    /// Rotation, four floats per keyframe.
    Rotation,
    /// Scale, three floats per keyframe.
    Scale,
}

impl Property {
    /// How many floats one keyframe of this property takes.
    #[must_use]
    pub const fn stride(self) -> usize {
        match self {
            Self::Translation | Self::Scale => 3,
            Self::Rotation => 4,
        }
    }
}

/// One joint's animation of one property.
#[derive(Debug, Clone, PartialEq)]
pub struct Channel {
    /// Which joint, as an index into [`Skin::joints`].
    pub joint: u8,
    /// What it drives.
    pub property: Property,
    /// Keyframe times, in seconds, ascending.
    pub times: Vec<f32>,
    /// Keyframe values, `times.len() * property.stride()` of them.
    pub values: Vec<f32>,
}

/// A named animation.
#[derive(Debug, Clone, PartialEq)]
pub struct Clip {
    /// What the file calls it. The engine's rig ships `idle`, `walk`, `run`,
    /// `swing` and `swim`, which is what an [`crate::ent::AnimTag`] maps onto.
    pub name: String,
    /// Length in seconds — the largest keyframe time in any channel.
    pub duration: f32,
    /// Channels, in file order.
    pub channels: Vec<Channel>,
}

/// A mesh, its skeleton and its clips.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Model {
    /// Vertices, in one buffer for the whole model.
    pub vertices: Vec<Vertex>,
    /// Triangle indices into [`Model::vertices`].
    pub indices: Vec<u32>,
    /// The skeleton, empty for a model with no skin.
    pub skin: Skin,
    /// Clips, in file order.
    pub clips: Vec<Clip>,
}

impl Model {
    /// The clip with this name, if it has one.
    #[must_use]
    pub fn clip(&self, name: &str) -> Option<&Clip> {
        self.clips.iter().find(|clip| clip.name == name)
    }
}

/// What a model is allowed to be, before any of it is allocated.
///
/// # Why these are a struct and not constants
///
/// Because the engine's own rig is loaded through the same path as a mod's, and
/// a fuzz target wants to drive the limits rather than the defaults. The
/// defaults are what a server's push gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Largest `.glb` accepted, in bytes.
    pub file_bytes: usize,
    /// Most vertices in one model.
    pub vertices: usize,
    /// Most indices in one model.
    pub indices: usize,
    /// Most joints in one skeleton.
    ///
    /// Bounded by more than taste: [`Vertex::joints`] is `u8`, so 255 is the
    /// ceiling the format can express at all, and a joint matrix per joint is
    /// what the GPU has to hold per drawn entity.
    pub joints: usize,
    /// Most nodes in the document.
    pub nodes: usize,
    /// Most clips.
    pub clips: usize,
    /// Most channels across all clips.
    pub channels: usize,
    /// Most keyframes in one channel.
    pub keyframes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            // Two megabytes. The engine's humanoid is a few kilobytes; this is
            // generous for a detailed mod model and small enough that a
            // hundred of them is not a hundred megabytes of JSON to parse.
            file_bytes: 2 * 1024 * 1024,
            // A character, not a scene. Sixty-four thousand triangles is far
            // past what a mob needs at the distance anyone sees one.
            vertices: 64 * 1024,
            indices: 192 * 1024,
            // A humanoid is about twenty. Sixty-four leaves room for fingers
            // and a tail without letting a file ask for a matrix palette that
            // does not fit a uniform buffer.
            joints: 64,
            nodes: 1024,
            clips: 32,
            channels: 512,
            // Sixty seconds at 60 Hz for one joint's one property.
            keyframes: 4096,
        }
    }
}
