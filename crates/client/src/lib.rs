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
pub mod audio;
pub mod cache;
pub mod camera;
pub mod config;
pub mod dialog;
pub mod entities;
pub mod front;
pub mod icons;
pub mod input;
pub mod launcher;
pub mod mesher;
pub mod net;
pub mod predict;
pub mod render;
pub mod shade;
pub mod shape_view;
pub mod sky;
pub mod texture;
pub mod trust;

/// The shape every full-screen panel takes: centred, four by three, most of the
/// window.
///
/// # Why one function rather than a number in each panel
///
/// The menu, the controls page, the inventory and a mod's dialog are all the
/// same kind of thing — a sheet the game puts in front of you — and a player
/// reads them as one system or as four. They were four: each picked its own
/// size, so opening the inventory after the settings moved the frame and
/// resized the content under the cursor.
///
/// **Four by three rather than the window's own ratio.** A panel that stretched
/// to an ultrawide monitor would put its two halves a foot apart; one that
/// matched a tall window would be a column. Four by three is the shape a page
/// of controls or a grid of slots actually wants, and it is the same shape on
/// every screen.
///
/// Three quarters of the window's HEIGHT, then width from the ratio, then
/// clamped so a narrow window cannot push the sides off the screen.
pub mod panel {
    /// How much of the window's height a panel takes.
    const SHARE: f32 = 0.75;
    /// Width over height.
    const RATIO: f32 = 4.0 / 3.0;
    /// The most of the window's width a panel may take, so it never touches the
    /// edges on a 4:3 monitor.
    const WIDEST: f32 = 0.9;

    /// The panel's size in points, for a window `area` points across.
    #[must_use]
    pub fn size(area: (f32, f32)) -> (f32, f32) {
        let height = (area.1 * SHARE).max(120.0);
        let width = (height * RATIO).min(area.0 * WIDEST).max(160.0);
        // Height follows the clamped width, so a narrow window keeps the ratio
        // rather than keeping the height and losing the shape.
        (width, (width / RATIO).min(area.1 * SHARE).max(120.0))
    }

    /// The panel's top-left corner in points, centred in `area`.
    #[must_use]
    pub fn origin(area: (f32, f32)) -> (f32, f32) {
        let (width, height) = size(area);
        ((area.0 - width) / 2.0, (area.1 - height) / 2.0)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_panel_keeps_its_shape_and_its_place_on_every_window() {
            for area in [
                (1920.0, 1080.0),
                (2560.0, 1080.0),
                (1024.0, 768.0),
                (800.0, 600.0),
            ] {
                let (width, height) = size(area);
                let ratio = width / height;
                assert!(
                    (ratio - RATIO).abs() < 0.01,
                    "a {area:?} window gave a {ratio} panel, which is not four by three"
                );
                assert!(width <= area.0, "the panel is wider than the window");
                assert!(height <= area.1, "the panel is taller than the window");

                // Centred: the space left over is the same on both sides.
                let (x, y) = origin(area);
                assert!((x - (area.0 - width - x)).abs() < 0.01);
                assert!((y - (area.1 - height - y)).abs() < 0.01);
            }
        }

        #[test]
        fn an_ultrawide_window_does_not_stretch_the_panel() {
            // The case the ratio exists for: the panel is the same width on a
            // 21:9 monitor as on a 16:9 one of the same height.
            let wide = size((3440.0, 1440.0));
            let normal = size((2560.0, 1440.0));
            assert!((wide.0 - normal.0).abs() < 0.01);
        }
    }
}
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
