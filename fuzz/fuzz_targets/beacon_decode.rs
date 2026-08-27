// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Fuzzes the LAN discovery beacon parser.
//!
//! **Charter rule 14: a parser ships with its fuzz target, in the same task.**
//! This one reads an OPEN UDP PORT — the only parser in the engine that runs
//! against bytes from a machine nobody chose to talk to. Anything on the
//! network can send anything to it, including before a player has decided to
//! join anything at all.
//!
//! The property is that no datagram panics and that decoding is total: every
//! input is either a beacon or `None`. Then the round trip, which is where a
//! format grows two spellings for one message — a decoder that ignored
//! trailing bytes or truncated an over-long name would accept a datagram this
//! crate could never have produced.
//!
//! Run: `cargo +nightly fuzz run beacon_decode`
#![no_main]

use libfuzzer_sys::fuzz_target;
use tiamot_core::discover::Beacon;

fuzz_target!(|data: &[u8]| {
    let Some(beacon) = Beacon::decode(data) else {
        return;
    };

    // A name that reached a caller must be one the encoder would have sent:
    // the list this ends up in is drawn from it, and a control character or a
    // direction override in there is somebody else's machine writing into this
    // player's screen.
    let bytes = beacon
        .encode()
        .expect("a beacon that decoded must be one this crate could send");
    let again = Beacon::decode(&bytes).expect("what this crate encoded, it must decode");
    assert_eq!(
        beacon, again,
        "a beacon decoded, re-encoded, and decoded to something else"
    );
    assert_eq!(
        bytes, data,
        "two different datagrams mean the same beacon, so the format is ambiguous"
    );
});
