// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Regenerates the `gltf_ingest` fuzz corpus seeds.
//!
//! Run: `cargo run --release -p tiamot-core --example gltf_seeds -- fuzz/corpus/gltf_ingest`
//!
//! # Why the shipped rig is the seed
//!
//! Charter rule 14 asks for this by name. A fuzzer starting from random bytes
//! spends its whole budget failing the four-byte magic check and never reaches
//! the JSON parser, let alone the accessors — which is where every interesting
//! bug in a container format lives. Starting from a real file, a single flipped
//! byte lands *inside* a count, an offset or an index, which is the mutation
//! that finds things.
//!
//! The variants beside it are the shapes a rig can take that the humanoid does
//! not: no skin at all, no clips, and a skeleton whose parents are written
//! after their children. Each gives the mutator a different starting point in
//! the same parser.

use std::path::PathBuf;

use tiamot_core::model::{Model, build, humanoid};

fn main() {
    let mut args = std::env::args().skip(1);
    let out = args
        .next()
        .map_or_else(|| PathBuf::from("fuzz/corpus/gltf_ingest"), PathBuf::from);
    if let Err(err) = std::fs::create_dir_all(&out) {
        eprintln!("could not create `{}`: {err}", out.display());
        std::process::exit(1);
    }

    let rig = humanoid();

    // A static mesh: the same geometry with the skeleton and clips taken away,
    // which is the whole no-skin branch of the reader.
    let bare = Model {
        vertices: rig.vertices.clone(),
        indices: rig.indices.clone(),
        ..Model::default()
    };

    // Rigged but still: exercises the skin path with the animation path empty.
    let posed = Model {
        clips: Vec::new(),
        ..rig.clone()
    };

    let seeds: [(&str, Vec<u8>); 3] = [
        ("humanoid", build::to_glb(&rig)),
        ("static", build::to_glb(&bare)),
        ("unanimated", build::to_glb(&posed)),
    ];

    let mut written = 0;
    for (name, bytes) in seeds {
        let path = out.join(format!("{name}.glb"));
        match std::fs::write(&path, &bytes) {
            Ok(()) => written += 1,
            Err(err) => eprintln!("could not write `{}`: {err}", path.display()),
        }
    }
    println!("wrote {written} seeds to {}", out.display());
}
