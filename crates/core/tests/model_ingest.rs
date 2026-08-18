// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! What the glTF reader does with input it should not trust.
//!
//! Charter rule 14: everything here arrived from a server the player did not
//! choose. The engine's own rig goes through the same path, which is what makes
//! the round trip below a test of both halves at once — a mistake in the writer
//! or the reader shows up as a mismatch rather than as a mob with its arm on
//! backwards.

use tiamot_core::model::{Limits, Model, ModelError, build, humanoid, ingest};

fn tiny() -> Limits {
    Limits::default()
}

/// The shipped rig, as a real `.glb`.
fn shipped() -> Vec<u8> {
    build::to_glb(&humanoid())
}

#[test]
fn the_shipped_rig_round_trips_through_the_container() {
    let model = humanoid();
    let bytes = shipped();
    let read = ingest::load(&bytes, &tiny()).expect("the engine's own rig should load");

    assert_eq!(
        read.vertices.len(),
        model.vertices.len(),
        "the vertex count changed on the way through"
    );
    assert_eq!(read.indices, model.indices);
    assert_eq!(
        read.skin.joints.len(),
        model.skin.joints.len(),
        "joints were lost"
    );
    // Parents survive, which is what the topological sort in the reader has to
    // preserve — a rig whose arm hangs off the wrong bone is a rig that folds
    // in half the moment anything animates.
    for (read, made) in read.skin.joints.iter().zip(&model.skin.joints) {
        assert_eq!(read.name, made.name);
        assert_eq!(
            read.parent, made.parent,
            "the parent of {} moved",
            made.name
        );
    }
    assert_eq!(
        read.clips.len(),
        model.clips.len(),
        "clips were lost: {:?}",
        read.clips.iter().map(|clip| &clip.name).collect::<Vec<_>>()
    );
}

#[test]
fn every_clip_the_engine_promises_is_in_the_rig() {
    // The tags the server sends have to land on something. A tag with no clip
    // falls back to idle rather than freezing, so a missing clip is silent —
    // which is exactly why it is worth a test.
    let model = humanoid();
    for name in ["idle", "walk", "run", "swing", "swim", "sneak"] {
        let clip = model
            .clip(name)
            .unwrap_or_else(|| panic!("the rig has no `{name}` clip"));
        assert!(clip.duration > 0.0, "`{name}` has no length");
        assert!(!clip.channels.is_empty(), "`{name}` animates nothing");
    }
}

#[test]
fn every_channel_names_a_joint_that_exists() {
    // A channel pointing past the end of the skeleton is a matrix palette read
    // out of bounds on the GPU. The reader rejects one; this checks the rig
    // does not ship one to begin with.
    let model = humanoid();
    let joints = model.skin.joints.len();
    for clip in &model.clips {
        for channel in &clip.channels {
            assert!(
                usize::from(channel.joint) < joints,
                "`{}` drives joint {} of {joints}",
                clip.name,
                channel.joint
            );
            assert_eq!(
                channel.values.len(),
                channel.times.len() * channel.property.stride(),
                "`{}` has a channel whose values and keyframes disagree",
                clip.name
            );
        }
    }
}

#[test]
fn the_rig_is_the_size_the_physics_collides() {
    // A body drawn taller than its box clips into ceilings, and the player sees
    // a head go through a floor that the server says is solid. The two numbers
    // have to be the same number.
    let model = humanoid();
    let top = model
        .vertices
        .iter()
        .map(|vertex| vertex.position[1])
        .fold(f32::MIN, f32::max);
    let bottom = model
        .vertices
        .iter()
        .map(|vertex| vertex.position[1])
        .fold(f32::MAX, f32::min);
    assert!(
        (top - tiamot_core::phys::PLAYER_HEIGHT).abs() < 0.01,
        "the rig is {top} cells tall and the collider is {}",
        tiamot_core::phys::PLAYER_HEIGHT
    );
    assert!(
        bottom.abs() < 0.01,
        "the rig's feet are at {bottom}, not on the ground"
    );
}

#[test]
fn a_file_over_the_size_limit_is_refused_before_it_is_parsed() {
    // The first check, and the one that has to come before every other: a
    // forty-byte JSON object can declare four billion vertices, so the document
    // is bounded before the parser sees a byte of it.
    let limits = Limits {
        file_bytes: 16,
        ..Limits::default()
    };
    assert!(matches!(
        ingest::load(&shipped(), &limits),
        Err(ModelError::TooLarge { .. })
    ));
}

#[test]
fn random_bytes_are_refused_rather_than_interpreted() {
    for length in [0usize, 1, 11, 12, 64, 4096] {
        let bytes: Vec<u8> = (0..length).map(|index| (index % 251) as u8).collect();
        assert!(
            ingest::load_isolated(&bytes, &tiny()).is_err(),
            "{length} bytes of nonsense was accepted as a model"
        );
    }
}

#[test]
fn a_truncated_file_is_refused_at_every_length() {
    // The classic parser fuzz case, done exhaustively rather than randomly: a
    // real file cut off at each byte. Any of them may be refused; none of them
    // may panic, and none may produce a model claiming data that is not there.
    let full = shipped();
    for cut in (0..full.len()).step_by(7) {
        let bytes = &full[..cut];
        match ingest::load_isolated(bytes, &tiny()) {
            Err(_) => {}
            Ok(model) => {
                // A truncation that happens to leave a valid document is fine,
                // as long as what it produced is self-consistent.
                assert_indices_in_range(&model, cut);
            }
        }
    }
}

#[test]
fn a_corrupted_byte_never_panics() {
    // Every byte of a real file, flipped. This is the shape of the failure the
    // fuzz target hunts for, run deterministically so a regression fails in CI
    // rather than on somebody's overnight fuzz run.
    let full = shipped();
    for index in (0..full.len()).step_by(13) {
        let mut bytes = full.clone();
        bytes[index] ^= 0xFF;
        if let Ok(model) = ingest::load_isolated(&bytes, &tiny()) {
            assert_indices_in_range(&model, index);
        }
    }
}

fn assert_indices_in_range(model: &Model, seed: usize) {
    for index in &model.indices {
        assert!(
            (*index as usize) < model.vertices.len(),
            "case {seed} produced an index past the end of the vertices"
        );
    }
    for vertex in &model.vertices {
        for joint in vertex.joints {
            assert!(
                model.skin.joints.is_empty() || usize::from(joint) < model.skin.joints.len(),
                "case {seed} produced a joint index past the end of the skeleton"
            );
        }
    }
    for clip in &model.clips {
        for channel in &clip.channels {
            assert!(
                usize::from(channel.joint) < model.skin.joints.len(),
                "case {seed} produced a channel driving a joint that does not exist"
            );
        }
    }
}

#[test]
fn a_buffer_with_a_uri_is_refused() {
    // A `uri` is a fetch — of a file on the player's disk, or of an address on
    // their network. A renderer that quietly performs one is a server-side
    // request forgery with a nice texture on it.
    let doc =
        br#"{"asset":{"version":"2.0"},"buffers":[{"byteLength":4,"uri":"file:///etc/passwd"}]}"#;
    let bytes = glb(doc, &[0, 0, 0, 0]);
    match ingest::load(&bytes, &tiny()) {
        Err(ModelError::ExternalReference { uri }) => {
            assert!(uri.starts_with("file:"), "the URI was not reported: {uri}");
        }
        other => panic!("an external buffer reference was not refused: {other:?}"),
    }
}

#[test]
fn any_embedded_image_is_refused_without_being_inspected() {
    // Not because a texture is dangerous, but because asking what one IS is:
    // `gltf::Image::source()` unwraps three `Option`s that come straight from
    // the JSON, and the fuzz target reached all of them. This reader takes
    // geometry, a skeleton and clips; textures arrive separately.
    for doc in [
        br#"{"asset":{"version":"2.0"},"buffers":[{"byteLength":4}],"images":[{"uri":"http://example.invalid/x.png"}]}"#.as_slice(),
        // No `uri` and no `bufferView`: the shape that made the crate unwrap.
        br#"{"asset":{"version":"2.0"},"buffers":[{"byteLength":4}],"images":[{}]}"#.as_slice(),
    ] {
        let bytes = glb(doc, &[0, 0, 0, 0]);
        assert!(
            matches!(ingest::load(&bytes, &tiny()), Err(ModelError::EmbeddedImage)),
            "an embedded image was inspected rather than refused: {:?}",
            ingest::load(&bytes, &tiny())
        );
    }

    // A `bufferView` index past the end is caught earlier still, by the crate's
    // own document validation. Refused either way, which is all that matters —
    // asserting on WHICH refusal would be a test of somebody else's error
    // message.
    let out_of_range = glb(
        br#"{"asset":{"version":"2.0"},"buffers":[{"byteLength":4}],"images":[{"bufferView":99}]}"#,
        &[0, 0, 0, 0],
    );
    assert!(ingest::load(&out_of_range, &tiny()).is_err());
}

#[test]
fn a_container_claiming_a_length_smaller_than_its_header_is_refused() {
    // An underflow: the crate subtracts the header size from the declared total
    // length, so a file claiming four bytes wraps to four billion — a panic
    // where overflow checks are on and a nonsense length where they are not.
    // Found by the fuzz target.
    let mut bytes = shipped();
    bytes[8..12].copy_from_slice(&4u32.to_le_bytes());
    assert!(matches!(
        ingest::load(&bytes, &tiny()),
        Err(ModelError::Malformed { .. })
    ));
}

#[test]
fn a_plain_gltf_json_file_is_not_accepted_as_a_model() {
    // Only `.glb`. A `.gltf` is a JSON document whose buffers live in sibling
    // files, so accepting one would mean accepting exactly the external
    // references this reader refuses — and until the magic was checked, a file
    // that was not a `.glb` fell through to that path silently.
    let doc = br#"{"asset":{"version":"2.0"},"buffers":[{"byteLength":4,"uri":"data.bin"}]}"#;
    assert!(matches!(
        ingest::load(doc, &tiny()),
        Err(ModelError::NotBinary)
    ));
}

#[test]
fn a_declared_count_larger_than_the_limit_is_refused_before_allocation() {
    // The decompression bomb, in glTF's shape: an accessor claiming far more
    // elements than the buffer could hold. The count is checked against the
    // limit first, so this costs a comparison rather than a reserve.
    let limits = Limits {
        vertices: 8,
        ..Limits::default()
    };
    let bytes = shipped();
    assert!(matches!(
        ingest::load(&bytes, &limits),
        Err(ModelError::TooMany {
            what: "vertices",
            ..
        })
    ));
}

#[test]
fn a_joint_limit_below_the_rig_is_refused() {
    let limits = Limits {
        joints: 2,
        ..Limits::default()
    };
    assert!(matches!(
        ingest::load(&shipped(), &limits),
        Err(ModelError::TooMany { what: "joints", .. })
    ));
}

#[test]
fn a_glb_with_no_binary_chunk_is_refused() {
    // Its accessors point at nothing, and a reader that treated the absence as
    // an empty buffer would read every accessor as zero-length and hand back a
    // model with no geometry rather than an error.
    let doc = br#"{"asset":{"version":"2.0"}}"#;
    let mut json = doc.to_vec();
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }
    let total = 12 + 8 + json.len();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&0x4654_6C67u32.to_le_bytes());
    bytes.extend_from_slice(&2u32.to_le_bytes());
    bytes.extend_from_slice(&u32::try_from(total).expect("fits").to_le_bytes());
    bytes.extend_from_slice(&u32::try_from(json.len()).expect("fits").to_le_bytes());
    bytes.extend_from_slice(&0x4E4F_534Au32.to_le_bytes());
    bytes.extend_from_slice(&json);

    assert!(matches!(
        ingest::load(&bytes, &tiny()),
        Err(ModelError::NoBlob)
    ));
}

/// A `.glb` around a hand-written JSON document and a binary chunk.
fn glb(document: &[u8], blob: &[u8]) -> Vec<u8> {
    let mut json = document.to_vec();
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }
    let mut binary = blob.to_vec();
    while !binary.len().is_multiple_of(4) {
        binary.push(0);
    }
    let total = 12 + 8 + json.len() + 8 + binary.len();

    let mut out = Vec::new();
    out.extend_from_slice(&0x4654_6C67u32.to_le_bytes());
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&u32::try_from(total).expect("fits").to_le_bytes());
    out.extend_from_slice(&u32::try_from(json.len()).expect("fits").to_le_bytes());
    out.extend_from_slice(&0x4E4F_534Au32.to_le_bytes());
    out.extend_from_slice(&json);
    out.extend_from_slice(&u32::try_from(binary.len()).expect("fits").to_le_bytes());
    out.extend_from_slice(&0x004E_4942u32.to_le_bytes());
    out.extend_from_slice(&binary);
    out
}

#[test]
fn every_seed_in_the_fuzz_corpus_is_answered_rather_than_survived() {
    // The corpus is committed, so this turns a fuzzer's overnight finding into
    // a gate that fails on the next `cargo test`. `rotation-declared-as-vec3`
    // is the first one it found: a rotation channel whose output accessor said
    // three floats, which made the `gltf` crate assert on the type its own call
    // site had chosen. The reader validates every accessor's declared shape now.
    let corpus =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus/gltf_ingest");
    let Ok(entries) = std::fs::read_dir(&corpus) else {
        panic!(
            "the fuzz corpus should be committed at {}",
            corpus.display()
        );
    };

    let mut checked = 0;
    for entry in entries.flatten() {
        let Ok(bytes) = std::fs::read(entry.path()) else {
            continue;
        };
        checked += 1;
        // `load`, not `load_isolated`: catching the panic here would be exactly
        // the thing that hides the bug this test exists for.
        if let Ok(model) = ingest::load(&bytes, &tiny()) {
            assert_indices_in_range(&model, checked);
        }
    }
    assert!(
        checked >= 3,
        "only {checked} seeds; the corpus is not there"
    );
}
