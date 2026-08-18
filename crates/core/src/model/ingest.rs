// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Reading a `.glb`, on the assumption that whoever sent it is hostile.
//!
//! # Charter rule 14, in the place it was written for
//!
//! A client joins a stranger's server and that server hands it models. glTF is
//! the largest attack surface in the project: a container holding a JSON
//! document, a binary blob, and indices from one into the other, where every
//! index is a number somebody else chose. The rules:
//!
//! - **Pure Rust.** The `gltf` crate, with default features off — no `image`,
//!   no `import`, no filesystem, no base64. There is no C codec anywhere on
//!   this path and there will not be one.
//! - **Limits before allocation.** A forty-byte JSON object can declare four
//!   billion vertices. Every count is checked against [`Limits`] before the
//!   buffer it describes is touched, so a bomb costs a comparison rather than
//!   a `Vec` reserve.
//! - **Embedded only.** A buffer or an image with a `uri` is **refused**, not
//!   ignored. A `uri` is a fetch — of a file on the player's disk, or of an
//!   address on their network — and a renderer that quietly performs one is a
//!   server-side request forgery with a nice texture on it.
//! - **Accessors are bounds-checked against the blob** before any indexed read,
//!   and every joint index is checked against the skeleton. An index that
//!   points past the end is the classic way a parser reads somebody else's
//!   memory; here it is a rejection.
//! - **A poisoned model is a missing model, never a crash.** The container
//!   parse is wrapped in `catch_unwind` inside [`load`] itself, because the
//!   library's own validator indexes unchecked and cannot be made safe from
//!   outside; [`load_isolated`] wraps the rest for the same reason one step
//!   further out. The client draws a fallback with a per-server warning. One
//!   malformed file must not take a player out of the game.
//!
//! # What the fuzz target found, in its first ten minutes
//!
//! All three in the `gltf` crate rather than in this module, which is the
//! argument for fuzzing a pure-Rust dependency rather than trusting it:
//!
//! 1. A rotation channel whose output accessor declared three floats made the
//!    reader assert on the type its own call site had chosen. Fixed here by
//!    checking every accessor's declared shape before reading it.
//! 2. A container declaring a total length below its own header size underflows
//!    a subtraction. Fixed here by validating the header first.
//! 3. `Image::source()` unwraps three `Option`s straight from the JSON, and the
//!    document validator indexes `accessors` unchecked. Fixed here by never
//!    calling the first and isolating the second.
//!
//! Each one is now a committed corpus seed, and `model_ingest.rs` asserts the
//! whole corpus is answered rather than survived.
//!
//! The matching fuzz target is `fuzz/fuzz_targets/gltf_ingest.rs`, seeded with
//! the engine's own humanoid, and it lands in the same task as this file
//! because rule 14 says it must.
//!
//! # What is deliberately not read
//!
//! Materials, textures, cameras, lights, morph targets, scenes. A model is a
//! mesh, a skeleton and clips; everything else is a surface with no consumer,
//! and a parser that reads what nothing draws is attack surface for free.

use std::collections::BTreeMap;

use gltf::Semantic;
use gltf::accessor::DataType::{F32, I8, I16, U8, U16, U32};
use gltf::accessor::Dimensions::{Mat4, Scalar, Vec2, Vec3, Vec4};

use super::{Channel, Clip, Joint, Limits, Model, Pose, Property, Skin, Vertex};

/// Why a model was refused.
///
/// Every variant names the limit or the inconsistency, because the client turns
/// this into a per-server warning and "the model is broken" is not something a
/// player can report usefully.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ModelError {
    /// The file is larger than [`Limits::file_bytes`], checked before parsing.
    #[error("model is {bytes} bytes, over the {limit}-byte limit")]
    TooLarge {
        /// What arrived.
        bytes: usize,
        /// What is allowed.
        limit: usize,
    },

    /// Not a binary glTF at all.
    ///
    /// **Only `.glb` is accepted**, and refusing everything else here is what
    /// makes "self-contained" true by construction: a `.gltf` is a JSON file
    /// whose buffers live in sibling files, so accepting one would mean
    /// accepting the external references the next variant exists to refuse.
    #[error("model is not a binary glTF")]
    NotBinary,

    /// The container or its JSON would not parse.
    #[error("model is not a readable .glb: {detail}")]
    Malformed {
        /// What the parser said.
        detail: String,
    },

    /// A buffer or image named an external file.
    #[error("model refers to `{uri}`; only self-contained .glb files are accepted")]
    ExternalReference {
        /// What it asked for, truncated.
        uri: String,
    },

    /// The file carries embedded images, which this reader does not read.
    ///
    /// # Why refusing is right rather than merely convenient
    ///
    /// A model here is geometry, a skeleton and clips. Textures reach a client
    /// the way block textures already do — as separate, content-addressed files
    /// through the push pipeline — so an embedded image has no consumer, and
    /// charter rule 14's rule of thumb is that a format section nothing reads
    /// is attack surface for free.
    ///
    /// It is also not hypothetical. `gltf::Image::source()` unwraps three
    /// separate `Option`s that come straight from the JSON — an out-of-range
    /// buffer view, an absent MIME type, an absent URI — and the fuzz target
    /// reached all of it. Not calling it is a better answer than catching it.
    #[error("model carries embedded images; textures are pushed separately")]
    EmbeddedImage,

    /// The `.glb` had no binary chunk, so its accessors point at nothing.
    #[error("model has no embedded binary chunk")]
    NoBlob,

    /// Something in the file is bigger than [`Limits`] allows.
    #[error("model declares {found} {what}, over the limit of {limit}")]
    TooMany {
        /// Which count.
        what: &'static str,
        /// What it declared.
        found: usize,
        /// What is allowed.
        limit: usize,
    },

    /// An index pointed outside what it indexes.
    #[error("model has {what} index {index}, outside 0..{bound}")]
    OutOfRange {
        /// Which kind of index.
        what: &'static str,
        /// The offending value.
        index: usize,
        /// One past the largest legal value.
        bound: usize,
    },

    /// An accessor's declared shape is not the one its use requires.
    ///
    /// **Not a pedantic check.** The `gltf` crate's readers choose a Rust type
    /// from the call site and trust the accessor to match it: on a mismatch a
    /// debug build fires a `debug_assert` and a release build reads with the
    /// wrong stride and silently yields nothing. Neither is an answer to give a
    /// file somebody else wrote — and the fuzzer found the first one inside a
    /// minute of starting.
    #[error("model's {what} is declared {found}, which is not {wanted}")]
    WrongShape {
        /// Which attribute or accessor.
        what: &'static str,
        /// What the file said it was.
        found: String,
        /// What reading it requires.
        wanted: &'static str,
    },

    /// A sparse accessor, which nothing here reads.
    ///
    /// Refused rather than supported: sparse is a second, entirely separate
    /// path through the reader with its own index arithmetic, and no character
    /// exporter emits one. Charter rule 14's rule of thumb is that a format
    /// feature with no consumer is attack surface for free.
    #[error("model's {what} is a sparse accessor, which is not accepted")]
    Sparse {
        /// Which accessor.
        what: &'static str,
    },

    /// The file is internally inconsistent in a way no limit covers.
    #[error("model is inconsistent: {detail}")]
    Inconsistent {
        /// What does not add up.
        detail: &'static str,
    },

    /// The parser panicked. See [`load_isolated`].
    #[error("the model parser panicked")]
    Panicked,
}

/// Bytes in a `.glb` header: magic, version, total length.
const GLB_HEADER_BYTES: usize = 12;

/// Reads a self-contained `.glb`.
///
/// # Errors
///
/// [`ModelError`], naming the limit or the inconsistency.
pub fn load(bytes: &[u8], limits: &Limits) -> Result<Model, ModelError> {
    // **First, and before the parser sees a byte.** Everything below is bounded
    // by counts inside the document, and the document itself is bounded here.
    if bytes.len() > limits.file_bytes {
        return Err(ModelError::TooLarge {
            bytes: bytes.len(),
            limit: limits.file_bytes,
        });
    }

    // **The container header, before the library reads it.** Two things the
    // parser does not survive on its own, both found by the fuzz target within
    // minutes of it existing:
    //
    // - Without the magic check, a file that is not a `.glb` is treated as a
    //   plain `.gltf` — a JSON document whose buffers live in sibling files,
    //   which is the external reference this reader exists to refuse.
    // - The declared total length is subtracted from the header size, so a
    //   file claiming to be four bytes long **underflows**. In a build with
    //   overflow checks that is a panic; without them it is a length of four
    //   billion.
    if bytes.len() < GLB_HEADER_BYTES || bytes[..4] != *b"glTF" {
        return Err(ModelError::NotBinary);
    }
    let declared = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
    if declared < GLB_HEADER_BYTES || declared > bytes.len() {
        return Err(ModelError::Malformed {
            detail: format!(
                "the container declares {declared} bytes, and the file is {}",
                bytes.len()
            ),
        });
    }

    // **Every index reference in the document, before the library reads one.**
    // See `verify_references`: this is the check that keeps the parser out of
    // its own unchecked indexing, and it is the reason the fuzz target can go
    // green rather than being permanently amber.
    let json = serde_json::from_slice::<serde_json::Value>(json_chunk(bytes)?).map_err(|err| {
        ModelError::Malformed {
            detail: err.to_string(),
        }
    })?;
    verify_references(&json)?;
    verify_enumerations(&json)?;

    // **The container parse is isolated, and that is not belt-and-braces.**
    //
    // `gltf`'s own document validator indexes unchecked. A `POSITION` attribute
    // naming accessor 0 in a document with no accessors panics inside
    // `gltf-json`'s validation hook — before a single line of this module has
    // run, and with nothing this module could have checked first short of
    // reimplementing the validator. The fuzz target found it in six minutes,
    // after finding two more like it.
    //
    // So the parse itself is treated as the hostile operation it is. Charter
    // rule 14 asks for panic isolation on the asset path by name, and this is
    // where it earns its place: a poisoned model becomes a refusal with a
    // reason, and the client draws its fallback.
    let parsed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        gltf::Gltf::from_slice(bytes)
    }))
    .map_err(|_| ModelError::Malformed {
        detail: "the container parser panicked".to_owned(),
    })?;
    let gltf = parsed.map_err(|err| ModelError::Malformed {
        detail: err.to_string(),
    })?;
    let blob = gltf.blob.as_deref().ok_or(ModelError::NoBlob)?;

    // A `uri` on any buffer or image is a fetch. Refused before anything is
    // read, so a file that would have reached out is rejected whole rather than
    // partly loaded.
    for buffer in gltf.buffers() {
        if let gltf::buffer::Source::Uri(uri) = buffer.source() {
            return Err(ModelError::ExternalReference { uri: truncate(uri) });
        }
    }
    // Images are refused whole, and **without asking what their source is** —
    // see `ModelError::EmbeddedImage`. Merely calling `Image::source()` on a
    // hostile file is the bug.
    if gltf.images().next().is_some() {
        return Err(ModelError::EmbeddedImage);
    }

    check("nodes", gltf.nodes().len(), limits.nodes)?;
    check("clips", gltf.animations().len(), limits.clips)?;

    let (skin, by_node) = read_skin(&gltf, blob, limits)?;
    let (vertices, indices) = read_mesh(&gltf, blob, limits, skin.joints.len())?;
    let clips = read_clips(&gltf, blob, limits, &by_node)?;

    Ok(Model {
        vertices,
        indices,
        skin,
        clips,
    })
}

/// Reads a model, catching a panic rather than letting it reach the caller.
///
/// The parser has no `unsafe` of its own, and the `gltf` crate is pure Rust —
/// so this should never fire. It exists because "should never" is not "cannot",
/// and charter rule 14 asks for panic isolation on the asset path specifically:
/// a poisoned model disables that model, never the client.
///
/// # Errors
///
/// As [`load`], plus [`ModelError::Panicked`].
pub fn load_isolated(bytes: &[u8], limits: &Limits) -> Result<Model, ModelError> {
    // `AssertUnwindSafe` because the only state crossing the boundary is a
    // borrowed slice, which a panic cannot leave inconsistent.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| load(bytes, limits)))
        .unwrap_or(Err(ModelError::Panicked))
}

/// The JSON chunk of a `.glb`, bounds-checked.
///
/// The container's own framing, walked by hand rather than by the library,
/// because this is what the reference check below needs and because reading a
/// declared length out of a peer's file is exactly the arithmetic that has to be
/// done carefully.
fn json_chunk(bytes: &[u8]) -> Result<&[u8], ModelError> {
    let start = GLB_HEADER_BYTES + 8;
    if bytes.len() < start {
        return Err(ModelError::NotBinary);
    }
    let length = u32::from_le_bytes([
        bytes[GLB_HEADER_BYTES],
        bytes[GLB_HEADER_BYTES + 1],
        bytes[GLB_HEADER_BYTES + 2],
        bytes[GLB_HEADER_BYTES + 3],
    ]) as usize;
    let kind = &bytes[GLB_HEADER_BYTES + 4..start];
    if kind != b"JSON" {
        return Err(ModelError::NotBinary);
    }
    bytes
        .get(start..start.saturating_add(length))
        .ok_or(ModelError::Malformed {
            detail: "the JSON chunk runs past the end of the file".to_owned(),
        })
}

/// One index against the array it indexes.
///
/// A missing or non-numeric value is not an error here: the field is optional,
/// and a document whose types are wrong fails elsewhere with a better message.
fn one(
    value: Option<&serde_json::Value>,
    bound: usize,
    what: &'static str,
) -> Result<(), ModelError> {
    let Some(index) = value.and_then(serde_json::Value::as_u64) else {
        return Ok(());
    };
    let index = usize::try_from(index).unwrap_or(usize::MAX);
    if index >= bound {
        return Err(ModelError::OutOfRange { what, index, bound });
    }
    Ok(())
}

/// Every index in an array of them.
fn many(
    value: Option<&serde_json::Value>,
    bound: usize,
    what: &'static str,
) -> Result<(), ModelError> {
    let Some(list) = value.and_then(serde_json::Value::as_array) else {
        return Ok(());
    };
    for entry in list {
        one(Some(entry), bound, what)?;
    }
    Ok(())
}

/// The entries of a top-level array, or nothing.
fn section<'a>(json: &'a serde_json::Value, name: &str) -> &'a [serde_json::Value] {
    json.get(name)
        .and_then(serde_json::Value::as_array)
        .map_or(&[][..], Vec::as_slice)
}

/// Refuses a document whose indices point outside the arrays they index.
///
/// # Why this exists at all, given the library validates
///
/// Because the library's validator **indexes unchecked while validating**: a
/// `POSITION` attribute naming accessor 0 in a document with no accessors
/// panics inside `gltf-json`'s own hook. Skipping validation does not help —
/// `Primitive::get` then unwraps the same lookup — so the only way to keep the
/// parser out of its own unchecked indexing is to refuse the document before it
/// gets there.
///
/// It is also the check the task asks for by name: accessor bounds validated
/// before any indexed read. Doing it over the raw JSON rather than over the
/// parsed document is what makes "before" true.
fn verify_references(json: &serde_json::Value) -> Result<(), ModelError> {
    use serde_json::Value;

    let len = |name: &str| json.get(name).and_then(Value::as_array).map_or(0, Vec::len);
    let accessors = len("accessors");
    let views = len("bufferViews");
    let buffers = len("buffers");
    let nodes = len("nodes");
    let meshes = len("meshes");
    let skins = len("skins");
    let materials = len("materials");
    let images = len("images");
    let textures = len("textures");
    let samplers = len("samplers");
    let cameras = len("cameras");

    let each = |name: &str| section(json, name);

    for view in each("bufferViews") {
        one(view.get("buffer"), buffers, "bufferViews[].buffer")?;
    }
    for accessor in each("accessors") {
        // **A count of zero underflows** `stride * (count - 1)` inside the
        // library's iterator setup. The specification requires at least one
        // element; the fuzz target found the arithmetic before anyone read the
        // specification.
        if accessor.get("count").and_then(Value::as_u64) == Some(0) {
            return Err(ModelError::Inconsistent {
                detail: "an accessor declares no elements, which the format does not allow",
            });
        }
        one(accessor.get("bufferView"), views, "accessors[].bufferView")?;
        if let Some(sparse) = accessor.get("sparse") {
            one(
                sparse.pointer("/indices/bufferView"),
                views,
                "sparse indices bufferView",
            )?;
            one(
                sparse.pointer("/values/bufferView"),
                views,
                "sparse values bufferView",
            )?;
        }
    }
    for image in each("images") {
        one(image.get("bufferView"), views, "images[].bufferView")?;
    }
    for texture in each("textures") {
        one(texture.get("sampler"), samplers, "textures[].sampler")?;
        one(texture.get("source"), images, "textures[].source")?;
    }
    for node in each("nodes") {
        one(node.get("mesh"), meshes, "nodes[].mesh")?;
        one(node.get("skin"), skins, "nodes[].skin")?;
        one(node.get("camera"), cameras, "nodes[].camera")?;
        many(node.get("children"), nodes, "nodes[].children")?;
    }
    for scene in each("scenes") {
        many(scene.get("nodes"), nodes, "scenes[].nodes")?;
    }
    for skin in each("skins") {
        one(
            skin.get("inverseBindMatrices"),
            accessors,
            "skins[].inverseBindMatrices",
        )?;
        one(skin.get("skeleton"), nodes, "skins[].skeleton")?;
        many(skin.get("joints"), nodes, "skins[].joints")?;
    }
    verify_drawable_references(json, accessors, materials, textures, nodes)
}

/// The half of [`verify_references`] that walks meshes, materials and clips.
///
/// Split for length rather than for meaning: it is the same pass, and the bounds
/// it checks against are the ones its caller counted.
fn verify_drawable_references(
    json: &serde_json::Value,
    accessors: usize,
    materials: usize,
    textures: usize,
    nodes: usize,
) -> Result<(), ModelError> {
    use serde_json::Value;
    let each = |name: &str| section(json, name);

    for mesh in each("meshes") {
        let primitives = mesh
            .get("primitives")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        for primitive in primitives {
            if let Some(attributes) = primitive.get("attributes").and_then(Value::as_object) {
                for value in attributes.values() {
                    one(Some(value), accessors, "primitive attribute")?;
                }
            }
            one(primitive.get("indices"), accessors, "primitive indices")?;
            one(primitive.get("material"), materials, "primitive material")?;
            let targets = primitive
                .get("targets")
                .and_then(Value::as_array)
                .map_or(&[][..], Vec::as_slice);
            for target in targets {
                if let Some(entries) = target.as_object() {
                    for value in entries.values() {
                        one(Some(value), accessors, "morph target attribute")?;
                    }
                }
            }
        }
    }
    for material in each("materials") {
        // Every texture reference a material can carry, at whatever depth the
        // extension put it. A material is not read here, but the library's
        // validator walks one, so an out-of-range index in it is still a panic.
        for pointer in [
            "/pbrMetallicRoughness/baseColorTexture/index",
            "/pbrMetallicRoughness/metallicRoughnessTexture/index",
            "/normalTexture/index",
            "/occlusionTexture/index",
            "/emissiveTexture/index",
        ] {
            one(material.pointer(pointer), textures, "material texture")?;
        }
    }
    for animation in each("animations") {
        let own_samplers = animation
            .get("samplers")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        for sampler in own_samplers {
            one(sampler.get("input"), accessors, "animation sampler input")?;
            one(sampler.get("output"), accessors, "animation sampler output")?;
        }
        let channels = animation
            .get("channels")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        for channel in channels {
            // **The animation's OWN samplers, not the document's.** A channel's
            // `sampler` is a local index, and checking it against the global
            // `samplers` array — which is textures' samplers — would be a bound
            // that is both wrong and usually larger.
            one(
                channel.get("sampler"),
                own_samplers.len(),
                "animation channel sampler",
            )?;
            one(
                channel.pointer("/target/node"),
                nodes,
                "channel target node",
            )?;
        }
    }

    Ok(())
}

/// Refuses a document containing an enumerated value the library does not know.
///
/// # The same reason as [`verify_references`], one layer along
///
/// `gltf-json` stores an unrecognised enum as `Checked::Invalid` and calls
/// `.unwrap()` on it wherever it is read — a `componentType` of 7, a primitive
/// `mode` of 99, an animation path of `"wobble"`. The panic message is
/// literally "attempted to unwrap an invalid item", and the fuzz target reached
/// it after the reference check closed the previous three.
///
/// So every enumerated field is checked against the set the specification
/// defines, including the ones nothing here reads: the library's *validator*
/// walks materials, cameras and texture samplers whether this module does or
/// not.
#[allow(
    clippy::too_many_lines,
    reason = "a table of the specification's enumerations; splitting it would scatter the list"
)]
fn verify_enumerations(json: &serde_json::Value) -> Result<(), ModelError> {
    use serde_json::Value;

    fn number(
        value: Option<&Value>,
        allowed: &[u64],
        what: &'static str,
    ) -> Result<(), ModelError> {
        let Some(found) = value.and_then(Value::as_u64) else {
            return Ok(());
        };
        if allowed.contains(&found) {
            return Ok(());
        }
        Err(ModelError::WrongShape {
            what,
            found: found.to_string(),
            wanted: "a value the glTF specification defines",
        })
    }

    fn text(value: Option<&Value>, allowed: &[&str], what: &'static str) -> Result<(), ModelError> {
        let Some(found) = value.and_then(Value::as_str) else {
            return Ok(());
        };
        if allowed.contains(&found) {
            return Ok(());
        }
        Err(ModelError::WrongShape {
            what,
            found: found.chars().take(32).collect(),
            wanted: "a value the glTF specification defines",
        })
    }

    let each = |name: &str| -> &[Value] {
        json.get(name)
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice)
    };

    for accessor in each("accessors") {
        number(
            accessor.get("componentType"),
            &[5120, 5121, 5122, 5123, 5125, 5126],
            "accessor componentType",
        )?;
        text(
            accessor.get("type"),
            &["SCALAR", "VEC2", "VEC3", "VEC4", "MAT2", "MAT3", "MAT4"],
            "accessor type",
        )?;
    }
    for view in each("bufferViews") {
        number(view.get("target"), &[34962, 34963], "bufferView target")?;
    }
    for mesh in each("meshes") {
        let primitives = mesh
            .get("primitives")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        for primitive in primitives {
            number(
                primitive.get("mode"),
                &[0, 1, 2, 3, 4, 5, 6],
                "primitive mode",
            )?;
            if let Some(attributes) = primitive.get("attributes").and_then(Value::as_object) {
                for key in attributes.keys() {
                    if !known_semantic(key) {
                        return Err(ModelError::WrongShape {
                            what: "primitive attribute",
                            found: key.chars().take(32).collect(),
                            wanted: "a semantic the glTF specification defines",
                        });
                    }
                }
            }
        }
    }
    for animation in each("animations") {
        let channels = animation
            .get("channels")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        for channel in channels {
            text(
                channel.pointer("/target/path"),
                &["translation", "rotation", "scale", "weights"],
                "animation target path",
            )?;
        }
        let samplers = animation
            .get("samplers")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        for sampler in samplers {
            text(
                sampler.get("interpolation"),
                &["LINEAR", "STEP", "CUBICSPLINE"],
                "animation interpolation",
            )?;
        }
    }
    for material in each("materials") {
        text(
            material.get("alphaMode"),
            &["OPAQUE", "MASK", "BLEND"],
            "material alphaMode",
        )?;
    }
    for image in each("images") {
        text(
            image.get("mimeType"),
            &["image/jpeg", "image/png"],
            "image mimeType",
        )?;
    }
    for camera in each("cameras") {
        text(
            camera.get("type"),
            &["perspective", "orthographic"],
            "camera type",
        )?;
    }
    for sampler in each("samplers") {
        number(sampler.get("magFilter"), &[9728, 9729], "sampler magFilter")?;
        number(
            sampler.get("minFilter"),
            &[9728, 9729, 9984, 9985, 9986, 9987],
            "sampler minFilter",
        )?;
        for axis in ["wrapS", "wrapT"] {
            number(sampler.get(axis), &[33071, 33648, 10497], "sampler wrap")?;
        }
    }

    Ok(())
}

/// Whether a primitive attribute name is one the specification defines.
///
/// The indexed families take any number, which is the specification's own rule;
/// the reader only ever asks for set zero.
fn known_semantic(name: &str) -> bool {
    const PLAIN: [&str; 3] = ["POSITION", "NORMAL", "TANGENT"];
    const INDEXED: [&str; 4] = ["TEXCOORD", "COLOR", "JOINTS", "WEIGHTS"];

    if PLAIN.contains(&name) {
        return true;
    }
    INDEXED.iter().any(|family| {
        name.strip_prefix(family)
            .and_then(|rest| rest.strip_prefix('_'))
            .is_some_and(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
    })
}

/// Refuses an accessor whose declared shape is not the one about to be read.
///
/// Called before every read, and the reason is [`ModelError::WrongShape`].
fn expect(
    accessor: &gltf::Accessor,
    what: &'static str,
    dimensions: gltf::accessor::Dimensions,
    types: &[gltf::accessor::DataType],
    wanted: &'static str,
) -> Result<(), ModelError> {
    if accessor.sparse().is_some() {
        return Err(ModelError::Sparse { what });
    }
    if accessor.dimensions() != dimensions || !types.contains(&accessor.data_type()) {
        return Err(ModelError::WrongShape {
            what,
            found: format!("{:?}/{:?}", accessor.dimensions(), accessor.data_type()),
            wanted,
        });
    }
    Ok(())
}

/// A count against its limit.
fn check(what: &'static str, found: usize, limit: usize) -> Result<(), ModelError> {
    if found > limit {
        return Err(ModelError::TooMany { what, found, limit });
    }
    Ok(())
}

/// A URI as it goes into an error message.
///
/// Bounded, because it is a string from a peer and it ends up in a log and on a
/// player's screen. Sixty characters is enough to recognise what was asked for.
fn truncate(uri: &str) -> String {
    uri.chars().take(60).collect()
}

/// The skeleton, with parents ordered before children.
///
/// # Why the ordering is done here
///
/// glTF stores a node hierarchy in whatever order the exporter felt like, and
/// building a world matrix needs a joint's parent already done. Sorting once at
/// ingest turns every later evaluation into a single forward pass — no
/// recursion, no visited set, and no chance of a cycle at draw time, because a
/// cycle cannot survive this sort and is rejected below.
///
/// Returns the skeleton and a map from glTF node index to joint index, which is
/// what an animation channel needs: a channel names a NODE, and matching the
/// two by name instead would quietly drop every channel on an unnamed joint.
type JointsByNode = BTreeMap<usize, u8>;

fn read_skin(
    gltf: &gltf::Gltf,
    blob: &[u8],
    limits: &Limits,
) -> Result<(Skin, JointsByNode), ModelError> {
    let Some(skin) = gltf.skins().next() else {
        return Ok((Skin::default(), JointsByNode::new()));
    };

    let nodes: Vec<gltf::Node> = skin.joints().collect();
    check("joints", nodes.len(), limits.joints)?;
    if nodes.len() > usize::from(u8::MAX) {
        return Err(ModelError::TooMany {
            what: "joints",
            found: nodes.len(),
            limit: usize::from(u8::MAX),
        });
    }

    // Node index to position in the skin, so a child can name its parent by the
    // index this function is about to hand out.
    let position: BTreeMap<usize, usize> = nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (node.index(), index))
        .collect();

    // Every joint's parent, found by looking at who claims it as a child. glTF
    // has no parent pointer, so this is the only way round.
    let mut parent_of: BTreeMap<usize, usize> = BTreeMap::new();
    for node in gltf.nodes() {
        for child in node.children() {
            if let (Some(&parent), Some(&child)) =
                (position.get(&node.index()), position.get(&child.index()))
            {
                parent_of.insert(child, parent);
            }
        }
    }

    if let Some(accessor) = skin.inverse_bind_matrices() {
        expect(&accessor, "inverseBindMatrices", Mat4, &[F32], "16 floats")?;
    }
    let inverse_binds: Vec<[f32; 16]> = skin
        .reader(|_| Some(blob))
        .read_inverse_bind_matrices()
        .map(|matrices| matrices.map(flatten).collect())
        .unwrap_or_default();
    if !inverse_binds.is_empty() && inverse_binds.len() != nodes.len() {
        return Err(ModelError::Inconsistent {
            detail: "the skin has a different number of inverse bind matrices than joints",
        });
    }

    // Topological order: repeatedly take every joint whose parent is already
    // placed. A joint left over after a full pass with nothing placed is in a
    // cycle, which is the one shape that would make a forward evaluation loop
    // for ever.
    let mut order: Vec<usize> = Vec::with_capacity(nodes.len());
    let mut placed = vec![false; nodes.len()];
    while order.len() < nodes.len() {
        let before = order.len();
        for index in 0..nodes.len() {
            if placed[index] {
                continue;
            }
            let ready = match parent_of.get(&index) {
                None => true,
                Some(parent) => placed[*parent],
            };
            if ready {
                placed[index] = true;
                order.push(index);
            }
        }
        if order.len() == before {
            return Err(ModelError::Inconsistent {
                detail: "the skeleton has a cycle, so no joint can be evaluated first",
            });
        }
    }

    // Old index to new, now that the order is known.
    let mut moved = vec![0u8; nodes.len()];
    for (new, old) in order.iter().enumerate() {
        moved[*old] = u8::try_from(new).unwrap_or(0);
    }

    let mut joints = Vec::with_capacity(order.len());
    for old in order {
        let node = &nodes[old];
        let (translation, rotation, scale) = node.transform().decomposed();
        joints.push(Joint {
            name: node.name().map(truncate).unwrap_or_default(),
            parent: parent_of.get(&old).map(|parent| moved[*parent]),
            rest: Pose {
                translation,
                rotation,
                scale,
            },
            inverse_bind: inverse_binds.get(old).copied().unwrap_or(IDENTITY),
        });
    }

    let by_node: JointsByNode = position
        .iter()
        .map(|(node, index)| (*node, moved[*index]))
        .collect();

    Ok((Skin { joints }, by_node))
}

/// The identity matrix, for a skin that shipped no inverse binds.
const IDENTITY: [f32; 16] = [
    1.0, 0.0, 0.0, 0.0, //
    0.0, 1.0, 0.0, 0.0, //
    0.0, 0.0, 1.0, 0.0, //
    0.0, 0.0, 0.0, 1.0,
];

fn flatten(rows: [[f32; 4]; 4]) -> [f32; 16] {
    let mut out = [0.0; 16];
    for (row, values) in rows.iter().enumerate() {
        out[row * 4..row * 4 + 4].copy_from_slice(values);
    }
    out
}

/// Every primitive of every mesh, concatenated into one buffer.
///
/// One buffer because the client draws a model in one call: a rig split across
/// primitives is an exporter's decision about materials, and this engine has
/// one material per model.
#[allow(
    clippy::too_many_lines,
    reason = "one linear pass per primitive; splitting it would hide the order of the checks"
)]
fn read_mesh(
    gltf: &gltf::Gltf,
    blob: &[u8],
    limits: &Limits,
    joints: usize,
) -> Result<(Vec<Vertex>, Vec<u32>), ModelError> {
    let mut vertices: Vec<Vertex> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();

    for mesh in gltf.meshes() {
        for primitive in mesh.primitives() {
            if primitive.mode() != gltf::mesh::Mode::Triangles {
                // Strips, fans and lines. Refusing rather than converting: a
                // format with one topology has one set of index rules to get
                // right, and nothing here needs the others.
                continue;
            }
            // **Every accessor is checked before the reader touches it.** See
            // `expect`: the library trusts the file's declared shape to match
            // the type its call site chose, and a file that does not match is
            // the whole reason this function exists.
            let checks: [(
                Semantic,
                &'static str,
                gltf::accessor::Dimensions,
                &[_],
                &str,
            ); 5] = [
                (Semantic::Positions, "POSITION", Vec3, &[F32], "3 floats"),
                (Semantic::Normals, "NORMAL", Vec3, &[F32], "3 floats"),
                (
                    Semantic::TexCoords(0),
                    "TEXCOORD_0",
                    Vec2,
                    &[F32, U8, U16],
                    "2 floats or normalised integers",
                ),
                (
                    Semantic::Joints(0),
                    "JOINTS_0",
                    Vec4,
                    &[U8, U16],
                    "4 unsigned integers",
                ),
                (
                    Semantic::Weights(0),
                    "WEIGHTS_0",
                    Vec4,
                    &[F32, U8, U16],
                    "4 floats or normalised integers",
                ),
            ];
            for (semantic, what, dimensions, types, wanted) in checks {
                if let Some(accessor) = primitive.get(&semantic) {
                    expect(&accessor, what, dimensions, types, wanted)?;
                }
            }
            if let Some(accessor) = primitive.indices() {
                expect(
                    &accessor,
                    "indices",
                    Scalar,
                    &[U8, U16, U32],
                    "one unsigned integer",
                )?;
            }

            let reader = primitive.reader(|_| Some(blob));

            let Some(positions) = reader.read_positions() else {
                return Err(ModelError::Inconsistent {
                    detail: "a primitive has no positions",
                });
            };

            // **Counted before it is collected.** `read_positions` is a lazy
            // iterator over a declared accessor, so its length is a number from
            // the file — which is exactly the number that must be checked
            // before a `Vec` is reserved for it.
            let count = positions.len();
            check("vertices", vertices.len() + count, limits.vertices)?;

            let base = u32::try_from(vertices.len()).map_err(|_| ModelError::TooMany {
                what: "vertices",
                found: vertices.len(),
                limit: limits.vertices,
            })?;

            let normals: Vec<[f32; 3]> = reader
                .read_normals()
                .map(Iterator::collect)
                .unwrap_or_default();
            let uvs: Vec<[f32; 2]> = reader
                .read_tex_coords(0)
                .map(|uv| uv.into_f32().collect())
                .unwrap_or_default();
            let bones: Vec<[u16; 4]> = reader
                .read_joints(0)
                .map(|joints| joints.into_u16().collect())
                .unwrap_or_default();
            let weights: Vec<[f32; 4]> = reader
                .read_weights(0)
                .map(|weights| weights.into_f32().collect())
                .unwrap_or_default();

            for (index, position) in positions.enumerate() {
                let bone = bones.get(index).copied().unwrap_or([0; 4]);
                // Every joint index, against the skeleton that exists rather
                // than against the one the file claims. An out-of-range index
                // here would be read straight into a matrix palette lookup.
                let mut mapped = [0u8; 4];
                for (slot, value) in mapped.iter_mut().zip(bone) {
                    let value = usize::from(value);
                    if joints == 0 {
                        // No skin at all: a static mesh, and every influence
                        // must be the identity joint.
                        *slot = 0;
                        continue;
                    }
                    if value >= joints {
                        return Err(ModelError::OutOfRange {
                            what: "joint",
                            index: value,
                            bound: joints,
                        });
                    }
                    *slot = u8::try_from(value).unwrap_or(0);
                }

                vertices.push(Vertex {
                    position,
                    normal: normals.get(index).copied().unwrap_or([0.0, 1.0, 0.0]),
                    uv: uvs.get(index).copied().unwrap_or([0.0, 0.0]),
                    joints: mapped,
                    weights: weights.get(index).copied().unwrap_or([1.0, 0.0, 0.0, 0.0]),
                });
            }

            if let Some(read) = reader.read_indices() {
                {
                    let read: Vec<u32> = read.into_u32().collect();
                    check("indices", indices.len() + read.len(), limits.indices)?;
                    for index in read {
                        // Against what was actually read, not what was
                        // declared: a primitive may index only its own
                        // vertices, and `base` is where those start.
                        if usize::try_from(index).unwrap_or(usize::MAX) >= count {
                            return Err(ModelError::OutOfRange {
                                what: "vertex",
                                index: usize::try_from(index).unwrap_or(usize::MAX),
                                bound: count,
                            });
                        }
                        indices.push(base + index);
                    }
                }
            } else {
                // Unindexed: the vertices are the triangles, in order.
                check("indices", indices.len() + count, limits.indices)?;
                for offset in 0..count {
                    indices.push(base + u32::try_from(offset).unwrap_or(0));
                }
            }
        }
    }

    if !indices.len().is_multiple_of(3) {
        return Err(ModelError::Inconsistent {
            detail: "the index count is not a multiple of three, so the last triangle is partial",
        });
    }

    Ok((vertices, indices))
}

/// Every animation, as clips over the skeleton's joints.
fn read_clips(
    gltf: &gltf::Gltf,
    blob: &[u8],
    limits: &Limits,
    by_node: &JointsByNode,
) -> Result<Vec<Clip>, ModelError> {
    let mut clips = Vec::new();
    let mut total_channels = 0usize;

    for animation in gltf.animations() {
        let mut channels = Vec::new();
        let mut duration = 0.0f32;

        for channel in animation.channels() {
            total_channels += 1;
            check("channels", total_channels, limits.channels)?;

            let Some(&joint) = by_node.get(&channel.target().node().index()) else {
                // Animating something that is not a joint of this skin. Dropped
                // rather than refused: an exporter routinely emits a camera
                // track beside a character, and refusing the file over one
                // would reject perfectly good models.
                continue;
            };
            let property = match channel.target().property() {
                gltf::animation::Property::Translation => Property::Translation,
                gltf::animation::Property::Rotation => Property::Rotation,
                gltf::animation::Property::Scale => Property::Scale,
                // Morph targets, which nothing here draws.
                gltf::animation::Property::MorphTargetWeights => continue,
            };

            // The sampler's own accessors, before the reader picks a type for
            // them. A rotation channel declaring three floats is the exact
            // shape that made the library assert.
            let sampler = channel.sampler();
            expect(
                &sampler.input(),
                "animation input",
                Scalar,
                &[F32],
                "one float",
            )?;
            match property {
                Property::Rotation => expect(
                    &sampler.output(),
                    "rotation output",
                    Vec4,
                    &[F32, I8, U8, I16, U16],
                    "4 floats or normalised integers",
                )?,
                Property::Translation | Property::Scale => expect(
                    &sampler.output(),
                    "translation or scale output",
                    Vec3,
                    &[F32],
                    "3 floats",
                )?,
            }

            let reader = channel.reader(|_| Some(blob));
            let Some(times) = reader.read_inputs() else {
                continue;
            };
            let times: Vec<f32> = times.collect();
            check("keyframes", times.len(), limits.keyframes)?;

            let values: Vec<f32> = match reader.read_outputs() {
                // Translations and scales are both three floats a keyframe and
                // read identically; the property they drive is already decided
                // above, from the channel's own target.
                Some(
                    gltf::animation::util::ReadOutputs::Translations(values)
                    | gltf::animation::util::ReadOutputs::Scales(values),
                ) => values.flatten().collect(),
                Some(gltf::animation::util::ReadOutputs::Rotations(values)) => {
                    values.into_f32().flatten().collect()
                }
                _ => continue,
            };

            // The pairing the GPU will trust. A channel claiming ten keyframes
            // and shipping three sets of values is where a skinning shader
            // reads past the end of a buffer.
            if values.len() != times.len() * property.stride() {
                return Err(ModelError::Inconsistent {
                    detail: "a channel has a different number of values than keyframes",
                });
            }

            duration = duration.max(times.last().copied().unwrap_or(0.0));
            channels.push(Channel {
                joint,
                property,
                times,
                values,
            });
        }

        clips.push(Clip {
            name: animation.name().map(truncate).unwrap_or_default(),
            duration,
            channels,
        });
    }

    Ok(clips)
}
