// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Fuzzes the whole texture ingest path: bytes → validated → atlas slot.
//!
//! Charter rule 14's headline case. Every one of these bytes represents a PNG a
//! stranger's server pushed to a player's machine, and the decoder is the
//! attack surface.
//!
//! The property under test is **not** "decoding succeeds" — almost nothing here
//! will be a valid PNG. It is that the client survives every input: no panic,
//! no allocation on a peer's say-so, and a bounded result when it does decode.
//!
//! The path is fuzzed END TO END rather than just the decoder, because the
//! interesting failures are at the seams: a decoder that returns a 1×0 image,
//! a resample that divides by a zero dimension, an atlas blit that trusts the
//! image's own width field.
//!
//! Run: `cargo +nightly fuzz run texture_ingest`
#![no_main]

use client::texture::{Atlas, Image, MAX_DECODED_BYTES, MAX_DIMENSION, decode_png};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // `decode_png` rather than the isolated wrapper: `catch_unwind` would
    // swallow the very panic the fuzzer exists to find.
    let Ok(image) = decode_png(data) else {
        return;
    };

    // Anything that decodes must have honoured the limits. A decoder that
    // returned an oversized image would have already allocated it, so this
    // assertion is a backstop rather than the defence.
    assert!(
        image.width <= MAX_DIMENSION && image.height <= MAX_DIMENSION,
        "decoded {}x{}, over the limit",
        image.width,
        image.height
    );
    assert!(image.width > 0 && image.height > 0, "zero-sized image");

    let declared = u64::from(image.width) * u64::from(image.height) * 4;
    assert!(declared <= MAX_DECODED_BYTES, "decoded image over the byte limit");
    assert_eq!(
        image.rgba.len() as u64,
        declared,
        "the buffer does not match the declared dimensions"
    );

    // The rest of the path, on the decoded image: resampling and packing both
    // trust the image's dimensions, so they are part of what is being fuzzed.
    let tile = image.to_tile();
    assert_eq!(tile.rgba.len(), (tile.width as usize) * (tile.height as usize) * 4);

    let atlas = Atlas::build(&[Some(tile), None, Some(Image::missing())]);
    assert_eq!(
        atlas.image.rgba.len(),
        (atlas.side() as usize) * (atlas.side() as usize) * 4,
        "the atlas buffer does not match its own dimensions"
    );

    // UVs must stay inside the atlas, or the shader samples whatever is next to
    // it in GPU memory.
    for slot in 0..3 {
        let (u0, v0, u1, v1) = atlas.tile_uv(slot);
        for value in [u0, v0, u1, v1] {
            assert!((0.0..=1.0).contains(&value), "UV {value} outside the atlas");
        }
        assert!(u1 > u0 && v1 > v0, "degenerate UV rectangle");
    }
});
