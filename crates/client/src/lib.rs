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
//! Audio and UI land in Tasks 13 and 14.

#![warn(missing_docs)]
#![warn(clippy::pedantic)]
// Render code converts between pixel counts, vertex indices, and coordinates on
// almost every line, and the values involved are bounded by an atlas edge or a
// chunk extent — five orders of magnitude inside the types they live in.
// Annotating each site would bury the conversions that are genuinely worth a
// second look. Precision loss is expected and harmless here for the same
// reason charter rule 4 exempts presentation: nothing downstream of a pixel
// coordinate has to agree bit-for-bit with another machine.
// The mesher works in (u, v, w) plane coordinates and (x, y, z) cell
// coordinates, and those are the names the technique is described by
// everywhere it is written down. Spelling them out would make the code harder
// to check against the reference, not easier.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::many_single_char_names
)]

pub mod app;
pub mod cache;
pub mod camera;
pub mod config;
pub mod mesher;
pub mod net;
pub mod predict;
pub mod render;
pub mod shade;
pub mod texture;
pub mod trust;
pub mod world;

/// The engine's units-per-block constant, re-exported.
///
/// A one-line proof that the client links against the same `tiamot_core` the
/// server simulates with — charter rule 5's 27 units are not the client's to
/// decide.
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
