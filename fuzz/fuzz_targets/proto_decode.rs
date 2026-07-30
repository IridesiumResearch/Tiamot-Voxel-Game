// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Fuzzes the protocol decoder — the engine's front door.
//!
//! Every byte reaching `proto::decode` came from a peer, and the decoder cannot
//! tell a broken client from a hostile one. The property under test is not
//! "decoding succeeds" — most inputs should fail — but that **it always fails
//! by returning an error, never by panicking**, and never by allocating on a
//! peer's say-so.
//!
//! Run: `cargo +nightly fuzz run proto_decode`
#![no_main]

use libfuzzer_sys::fuzz_target;
use tiamot_core::proto::{ClientMessage, ServerMessage, decode, encode, validate_client_message};

fuzz_target!(|data: &[u8]| {
    // Both directions: a client parses server messages and vice versa, so both
    // decoders face untrusted input.
    if let Ok(message) = decode::<ClientMessage>(data) {
        // Anything that decodes must also survive validation and re-encoding
        // without panicking. Re-encoding matters because the server echoes
        // structure back to other clients.
        let _ = validate_client_message(&message);
        let _ = encode(&message);
    }

    if let Ok(message) = decode::<ServerMessage>(data) {
        let _ = encode(&message);
    }
});
