// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Fuzzes the fluid payload decoder.
//!
//! **Charter rule 14: a parser ships with its fuzz target, in the same task.**
//! A client decodes a chunk's fluid from a server it has no reason to trust,
//! and the decoder cannot tell a peer running a different build from one
//! probing it for a way in.
//!
//! The property is not that decoding succeeds — most random bytes are not a
//! valid payload and should be refused. It is that failure is always an error
//! return: never a panic, never an allocation sized by what the payload claims,
//! and never a loop that fails to terminate. A run-length format makes all
//! three reachable if the bounds are checked in the wrong order, which is why
//! the run lengths are summed against a chunk's size *before* anything is
//! written rather than after.
//!
//! Run: `cargo +nightly fuzz run fluid_decode`
#![no_main]

use libfuzzer_sys::fuzz_target;
use tiamot_core::fluid::codec::{decode, encode};

fuzz_target!(|data: &[u8]| {
    let Ok(layer) = decode(data) else {
        return;
    };

    // Anything that decodes must re-encode and decode again to the same thing.
    // A payload that survives the decoder but cannot be reproduced by the
    // encoder is a format the two halves disagree about, which on the wire is a
    // client and a server quietly holding different ponds — and the fluid layer
    // is hashed by the multiplayer test, so a disagreement here is a red CI leg
    // rather than something anybody would notice in play.
    let round_tripped = encode(&layer);
    let again = decode(&round_tripped).expect("what this crate encoded, it must decode");
    assert_eq!(
        layer, again,
        "a payload decoded, re-encoded, and decoded to something else"
    );
});
