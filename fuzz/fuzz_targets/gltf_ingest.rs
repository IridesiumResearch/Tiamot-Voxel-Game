// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Fuzzes the glTF reader — the largest hostile-input surface in the project.
//!
//! A `.glb` is a container holding a JSON document, a binary blob, and indices
//! from one into the other. Every one of those indices is a number somebody
//! else chose, and the reader's job is to refuse the ones that do not fit
//! rather than to follow them.
//!
//! The property is not "loading succeeds" — almost every input here is
//! nonsense and should be refused. It is:
//!
//! 1. **It never panics.** `load` is called directly rather than through
//!    `load_isolated`, because a fuzz target that caught its own panics would
//!    find nothing.
//! 2. **Nothing it returns points outside itself.** A model that loaded is a
//!    model the client will hand to a GPU: every triangle index inside the
//!    vertex list, every joint index inside the skeleton, every channel driving
//!    a joint that exists. An out-of-range index that survived the reader would
//!    be a read past the end of a buffer on the other side.
//! 3. **It obeys its limits.** A model over any of them is a model the reader
//!    was supposed to have refused before allocating for it.
//!
//! Seeded with the engine's own humanoid — see
//! `cargo run --release -p tiamot-core --example fuzz_seeds`. A fuzzer starting
//! from random bytes spends its whole budget failing the magic-number check.
//!
//! Run: `cargo +nightly fuzz run gltf_ingest`
#![no_main]

use libfuzzer_sys::fuzz_target;
use tiamot_core::model::{Limits, ingest};

fuzz_target!(|data: &[u8]| {
    let limits = Limits::default();
    let Ok(model) = ingest::load(data, &limits) else {
        return;
    };

    assert!(model.vertices.len() <= limits.vertices);
    assert!(model.indices.len() <= limits.indices);
    assert!(model.skin.joints.len() <= limits.joints);
    assert!(model.clips.len() <= limits.clips);

    for index in &model.indices {
        assert!(
            (*index as usize) < model.vertices.len(),
            "a triangle indexes a vertex that does not exist"
        );
    }
    for vertex in &model.vertices {
        for joint in vertex.joints {
            assert!(
                model.skin.joints.is_empty() || usize::from(joint) < model.skin.joints.len(),
                "a vertex is weighted to a joint that does not exist"
            );
        }
    }
    for (index, joint) in model.skin.joints.iter().enumerate() {
        if let Some(parent) = joint.parent {
            assert!(
                usize::from(parent) < index,
                "a joint's parent comes after it, so a forward pass cannot build it"
            );
        }
    }
    for clip in &model.clips {
        for channel in &clip.channels {
            assert!(
                usize::from(channel.joint) < model.skin.joints.len(),
                "a channel drives a joint that does not exist"
            );
            assert_eq!(
                channel.values.len(),
                channel.times.len() * channel.property.stride(),
                "a channel's values and keyframes disagree"
            );
            assert!(channel.times.len() <= limits.keyframes);
        }
    }
});
