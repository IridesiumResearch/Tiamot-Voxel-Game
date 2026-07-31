// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Client: a viewer onto a running server.
//!
//! Singleplayer is an embedded server over loopback, so this crate never
//! simulates anything itself (charter rule 2). It renders what
//! [`tiamot_core`] tells it and sends input actions back.
//!
//! Presentation code is explicitly exempt from the Deterministic Float Subset
//! (charter rule 4). Rendering, audio, UI layout, camera smoothing, and
//! client-side interpolation may use transcendentals freely — the determinism
//! rules apply to simulation, and taxing presentation with them buys nothing.
//!
//! Rendering, windowing, audio, and UI land in Tasks 08 onward.

/// Placeholder until Task 08 introduces the renderer.
///
/// Returns the engine's units-per-block constant so the crate has something
/// meaningful to test against `tiamot_core` before the renderer exists.
#[must_use]
pub fn units_per_block() -> u32 {
    tiamot_core::UNITS_PER_BLOCK
}

#[cfg(test)]
mod tests {
    #[test]
    fn links_against_core() {
        assert_eq!(super::units_per_block(), tiamot_core::UNITS_PER_BLOCK);
    }
}

pub mod mesher;
