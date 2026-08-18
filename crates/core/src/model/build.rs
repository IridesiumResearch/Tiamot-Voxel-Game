// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Writing a `.glb`, for the things that need one to exist.
//!
//! # Why an engine ships a glTF writer at all
//!
//! It does not need one to draw: [`super::Model`] is what the client uploads,
//! and [`super::humanoid`] builds the engine's own rig directly in Rust — as
//! source anyone can read and review, rather than as an opaque binary committed
//! to the repository.
//!
//! What needs a writer is everything that has to exercise the *reader*:
//!
//! - **The fuzz corpus.** Charter rule 14 asks for `fuzz/gltf_ingest` seeded
//!   with the shipped humanoid. A fuzzer starting from random bytes spends its
//!   whole budget failing the magic-number check; one starting from a real file
//!   starts inside the parser.
//! - **The tests.** A hostile-input parser is tested by handing it hostile
//!   input, and building a file that declares four billion vertices is a great
//!   deal easier with a writer than with a hex editor.
//! - **The round trip.** `load(to_glb(&model))` returning the same model is one
//!   assertion that covers both halves, and it is the reason a mistake in
//!   either shows up as a mismatch rather than as a mob with its arm on
//!   backwards.
//!
//! # This is deliberately a minimal writer
//!
//! One buffer, one buffer view per accessor, no materials, no textures, no
//! scenes beyond what a skin needs. It writes the subset [`super::ingest`]
//! reads, because a writer that emitted more would be generating test coverage
//! for a reader that does not have any.

use serde_json::{Value, json};

use super::{Model, Property};

/// The `.glb` magic number, `"glTF"` little-endian.
const MAGIC: u32 = 0x4654_6C67;

/// The container version this writes. glTF 2.0's `.glb` is version 2.
const VERSION: u32 = 2;

/// Chunk type `"JSON"`.
const CHUNK_JSON: u32 = 0x4E4F_534A;

/// Chunk type `"BIN\0"`.
const CHUNK_BIN: u32 = 0x004E_4942;

/// Accumulates the binary chunk and the accessors that point into it.
#[derive(Default)]
struct Blob {
    bytes: Vec<u8>,
    views: Vec<Value>,
    accessors: Vec<Value>,
}

impl Blob {
    /// Appends data and returns the accessor index for it.
    ///
    /// `component` and `kind` are glTF's own enumerations — 5126 is `FLOAT`,
    /// 5125 is `UNSIGNED_INT`, 5121 is `UNSIGNED_BYTE`.
    fn push(&mut self, data: &[u8], count: usize, component: u32, kind: &str) -> usize {
        // Every view starts at a four-byte boundary. glTF requires it for
        // anything the GPU reads directly, and a reader that assumed it would
        // otherwise pick up the tail of the previous accessor.
        while !self.bytes.len().is_multiple_of(4) {
            self.bytes.push(0);
        }
        let offset = self.bytes.len();
        self.bytes.extend_from_slice(data);

        let view = self.views.len();
        self.views.push(json!({
            "buffer": 0,
            "byteOffset": offset,
            "byteLength": data.len(),
        }));

        let accessor = self.accessors.len();
        self.accessors.push(json!({
            "bufferView": view,
            "componentType": component,
            "count": count,
            "type": kind,
        }));
        accessor
    }

    fn floats(&mut self, values: &[f32], components: usize, kind: &str) -> usize {
        let mut bytes = Vec::with_capacity(values.len() * 4);
        for value in values {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        self.push(&bytes, values.len() / components, 5126, kind)
    }
}

/// Encodes a model as a self-contained `.glb`.
#[must_use]
pub fn to_glb(model: &Model) -> Vec<u8> {
    let mut blob = Blob::default();

    let positions: Vec<f32> = model
        .vertices
        .iter()
        .flat_map(|vertex| vertex.position)
        .collect();
    let normals: Vec<f32> = model
        .vertices
        .iter()
        .flat_map(|vertex| vertex.normal)
        .collect();
    let uvs: Vec<f32> = model.vertices.iter().flat_map(|vertex| vertex.uv).collect();
    let weights: Vec<f32> = model
        .vertices
        .iter()
        .flat_map(|vertex| vertex.weights)
        .collect();
    let bones: Vec<u8> = model
        .vertices
        .iter()
        .flat_map(|vertex| vertex.joints)
        .collect();

    // `min`/`max` on POSITION are required by the specification, and a reader
    // is entitled to trust them for culling. Written honestly.
    let (min, max) = bounds(model);

    let position = blob.floats(&positions, 3, "VEC3");
    blob.accessors[position]["min"] = json!(min);
    blob.accessors[position]["max"] = json!(max);
    let normal = blob.floats(&normals, 3, "VEC3");
    let uv = blob.floats(&uvs, 2, "VEC2");
    let weight = blob.floats(&weights, 4, "VEC4");
    let joint = blob.push(&bones, model.vertices.len(), 5121, "VEC4");

    let mut indices = Vec::with_capacity(model.indices.len() * 4);
    for index in &model.indices {
        indices.extend_from_slice(&index.to_le_bytes());
    }
    let index = blob.push(&indices, model.indices.len(), 5125, "SCALAR");

    let mut attributes = json!({
        "POSITION": position,
        "NORMAL": normal,
        "TEXCOORD_0": uv,
    });
    if !model.skin.joints.is_empty() {
        attributes["JOINTS_0"] = json!(joint);
        attributes["WEIGHTS_0"] = json!(weight);
    }

    // Node 0 is the mesh; joints start at 1. A flat list, because the reader
    // finds parents by walking `children` rather than by position.
    let mut nodes = vec![json!({ "mesh": 0, "skin": 0 })];
    for (index, bone) in model.skin.joints.iter().enumerate() {
        let children: Vec<usize> = model
            .skin
            .joints
            .iter()
            .enumerate()
            .filter(|(_, child)| child.parent == u8::try_from(index).ok())
            .map(|(child, _)| child + 1)
            .collect();
        let mut node = json!({
            "name": bone.name,
            "translation": bone.rest.translation,
            "rotation": bone.rest.rotation,
            "scale": bone.rest.scale,
        });
        if !children.is_empty() {
            node["children"] = json!(children);
        }
        nodes.push(node);
    }

    let mut document = json!({
        "asset": { "version": "2.0", "generator": "tiamot" },
        "scene": 0,
        "scenes": [ { "nodes": [0] } ],
        "nodes": nodes,
        "meshes": [ {
            "primitives": [ { "attributes": attributes, "indices": index, "mode": 4 } ]
        } ],
    });

    if !model.skin.joints.is_empty() {
        document["skins"] = skins_of(model, &mut blob);
    }
    if !model.clips.is_empty() {
        document["animations"] = animations_of(model, &mut blob);
    }

    document["bufferViews"] = json!(blob.views);
    document["accessors"] = json!(blob.accessors);
    document["buffers"] = json!([ { "byteLength": blob.bytes.len() } ]);

    wrap(&document, &blob.bytes)
}

/// The one skin, and the inverse bind matrices it points at.
fn skins_of(model: &Model, blob: &mut Blob) -> Value {
    let binds: Vec<f32> = model
        .skin
        .joints
        .iter()
        .flat_map(|bone| bone.inverse_bind)
        .collect();
    let bind = blob.floats(&binds, 16, "MAT4");
    // Node 0 is the mesh, so joints start at 1.
    let joints: Vec<usize> = (1..=model.skin.joints.len()).collect();
    json!([ { "joints": joints, "inverseBindMatrices": bind } ])
}

/// Every clip, as an animation with its own samplers.
fn animations_of(model: &Model, blob: &mut Blob) -> Value {
    let mut animations = Vec::new();
    for clip in &model.clips {
        let mut samplers = Vec::new();
        let mut channels = Vec::new();
        for channel in &clip.channels {
            let times = blob.floats(&channel.times, 1, "SCALAR");
            let kind = match channel.property {
                Property::Rotation => "VEC4",
                Property::Translation | Property::Scale => "VEC3",
            };
            let values = blob.floats(&channel.values, channel.property.stride(), kind);
            channels.push(json!({
                "sampler": samplers.len(),
                "target": {
                    "node": usize::from(channel.joint) + 1,
                    "path": match channel.property {
                        Property::Translation => "translation",
                        Property::Rotation => "rotation",
                        Property::Scale => "scale",
                    },
                },
            }));
            samplers.push(json!({
                "input": times,
                "output": values,
                "interpolation": "LINEAR",
            }));
        }
        animations.push(json!({
            "name": clip.name,
            "channels": channels,
            "samplers": samplers,
        }));
    }
    json!(animations)
}

/// The model's axis-aligned bounds, for the required `POSITION` accessor hints.
fn bounds(model: &Model) -> ([f32; 3], [f32; 3]) {
    let mut min = [f32::MAX; 3];
    let mut max = [f32::MIN; 3];
    for vertex in &model.vertices {
        for axis in 0..3 {
            min[axis] = min[axis].min(vertex.position[axis]);
            max[axis] = max[axis].max(vertex.position[axis]);
        }
    }
    if model.vertices.is_empty() {
        return ([0.0; 3], [0.0; 3]);
    }
    (min, max)
}

/// Puts a JSON document and a binary blob into the `.glb` container.
fn wrap(document: &Value, blob: &[u8]) -> Vec<u8> {
    let mut json = serde_json::to_vec(document).unwrap_or_default();
    // Both chunks are padded to four bytes — the JSON with spaces and the
    // binary with zeroes, which is what the specification asks for by name.
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }
    let mut binary = blob.to_vec();
    while !binary.len().is_multiple_of(4) {
        binary.push(0);
    }

    let total = 12 + 8 + json.len() + 8 + binary.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&u32::try_from(total).unwrap_or(u32::MAX).to_le_bytes());

    out.extend_from_slice(&u32::try_from(json.len()).unwrap_or(0).to_le_bytes());
    out.extend_from_slice(&CHUNK_JSON.to_le_bytes());
    out.extend_from_slice(&json);

    out.extend_from_slice(&u32::try_from(binary.len()).unwrap_or(0).to_le_bytes());
    out.extend_from_slice(&CHUNK_BIN.to_le_bytes());
    out.extend_from_slice(&binary);

    out
}
