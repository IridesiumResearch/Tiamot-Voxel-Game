// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Scripted headless client, used to drive real servers in tests.
//!
//! Integration tests run bots against a real loopback server rather than a mock
//! (charter rule 15), so the tested path is the shipped path. A bug in framing,
//! in the handshake, or in the session state machine fails a test here rather
//! than surfacing the first time a human connects.
//!
//! Library-shaped on purpose: Task 07 builds the load-testing harness on top of
//! this, and a binary would have to be turned back into a library to get there.
//!
//! # Trust-on-first-use, without the "first use"
//!
//! A bot verifies the server's certificate fingerprint against the one it was
//! told to expect and refuses anything else. That is stricter than a real
//! client's TOFU — a bot always knows which server it meant to reach, because
//! the test started it — and it means the identity suite's "signature bound to
//! a different server" case exercises the real check rather than a bypass.

#![forbid(unsafe_code)]

pub mod bench;
pub mod client;
pub mod replay;
pub mod runner;
pub mod script;

pub use client::{Bot, BotError, Impairment};
pub use runner::{SwarmStats, drive, wander};
pub use script::{Channel, Command, Reply, ScriptOutcome, run_script};

/// The engine's chunk size, for tests that assert against it.
#[must_use]
pub fn chunk_blocks() -> u32 {
    tiamot_core::CHUNK_BLOCKS
}

#[cfg(test)]
mod tests {
    #[test]
    fn links_against_core() {
        assert_eq!(super::chunk_blocks(), tiamot_core::CHUNK_BLOCKS);
    }
}
