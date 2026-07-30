// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Scripted headless client, used to drive real servers in tests.
//!
//! Integration tests run bots against a real loopback server rather than a
//! mock (charter rule 15), so the tested path is the shipped path. The harness
//! proper arrives in Task 07.

/// Placeholder until Task 07 builds the bot harness.
///
/// Returns the engine's chunk size so the crate has something meaningful to
/// test against `tiamot_core` before the harness exists.
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
