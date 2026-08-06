// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Fuzzes the light payload decoder.
//!
//! **Charter rule 14: a parser ships with its fuzz target, in the same task.**
//! A client decodes chunk light from a server it has no reason to trust, and the
//! decoder cannot tell a peer with a different build from one probing it.
//!
//! The property is not that decoding succeeds — most random bytes are not a
//! valid payload, and should not be. It is that failure is always an error
//! return: never a panic, never an allocation sized by the payload, and never a
//! loop that does not terminate. The run-length format makes all three
//! reachable if the bounds are checked in the wrong order, which is why they are
//! checked before anything is written.
//!
//! Run: `cargo +nightly fuzz run light_decode`
#![no_main]

use libfuzzer_sys::fuzz_target;
use tiamot_core::light::codec::{decode, encode};

fuzz_target!(|data: &[u8]| {
    let Ok(layer) = decode(data) else {
        return;
    };

    // Anything that decodes must re-encode and decode again to the same thing.
    // A payload that survives the decoder but cannot be reproduced by the
    // encoder is a format the two halves disagree about, which on the wire is a
    // client and server that quietly hold different worlds.
    let round_tripped = encode(&layer);
    let again = decode(&round_tripped).expect("what this crate encoded, it must decode");
    assert_eq!(
        layer, again,
        "a payload decoded, re-encoded, and decoded to something else"
    );
});
