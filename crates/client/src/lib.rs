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

/// Interface controls whose behaviour is shared between screens.
pub mod widget {
    /// What a settled slider should do with the value it is showing.
    ///
    /// Returns the draft to keep for the next frame, and the value to apply, in
    /// that order. Exactly one of them is `Some` — a value is either still
    /// being chosen or it has been chosen.
    ///
    /// # Why a value is not applied while the drag is running
    ///
    /// **Because the interface scale rescales the slider.** Applying it live
    /// changes egui's zoom factor mid-drag, which moves the slider under the
    /// pointer, which changes the value the pointer is now over. Reported from
    /// the window as jumping and jerking around, and it is a feedback loop
    /// rather than a jitter: the control is an input to its own position.
    ///
    /// The previous rule was that a scale you cannot see while dragging is a
    /// scale you have to guess at. That is true and it is the lesser problem.
    ///
    /// A change that is not a drag — an arrow key, a click on the track —
    /// applies at once, because there is no drag to wait for the end of.
    #[must_use]
    pub const fn settle(
        dragging: bool,
        changed: bool,
        draft: Option<f32>,
        shown: f32,
    ) -> (Option<f32>, Option<f32>) {
        if dragging {
            return (Some(shown), None);
        }
        match draft {
            // The frame the pointer came up: what was being dragged is now the
            // answer, whether or not egui calls this frame a change.
            Some(value) => (None, Some(value)),
            None if changed => (None, Some(shown)),
            None => (None, None),
        }
    }

    /// A slider that applies its value only once the player lets go.
    ///
    /// `live` is what is in force now and `draft` is where the pointer has
    /// dragged to and not yet let go of. Returns a value the moment it is
    /// settled, and nothing on the frames in between — see [`settle`].
    pub fn on_release(
        ui: &mut egui::Ui,
        label: &str,
        range: std::ops::RangeInclusive<f32>,
        step: f64,
        live: f32,
        draft: &mut Option<f32>,
    ) -> Option<f32> {
        let mut shown = draft.unwrap_or(live);
        let response = ui.add(
            egui::Slider::new(&mut shown, range)
                .step_by(step)
                .text(label),
        );
        // **Held, not merely moved.** A pointer pressed on the handle and not
        // yet moved is a drag that has started, and treating it as settled
        // would apply a value on the way past.
        let holding = response.dragged() || response.is_pointer_button_down_on();
        let (kept, settled) = settle(holding, response.changed(), *draft, shown);
        *draft = kept;
        settled
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_drag_is_kept_and_a_release_is_applied() {
            // Nothing happening: no draft, no value.
            assert_eq!(settle(false, false, None, 1.0), (None, None));

            // The drag: remembered, and nothing applied yet — this is the whole
            // of the fix, because applying here is what moved the slider.
            assert_eq!(settle(true, true, None, 0.9), (Some(0.9), None));
            assert_eq!(settle(true, true, Some(0.9), 0.85), (Some(0.85), None));

            // The pointer comes up. egui reports no change on that frame, and
            // the answer is the draft rather than nothing.
            assert_eq!(settle(false, false, Some(0.85), 0.85), (None, Some(0.85)));

            // An arrow key or a click on the track: no drag to wait for.
            assert_eq!(settle(false, true, None, 1.1), (None, Some(1.1)));
        }
    }
}

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

    /// What a screen the player pressed a button to open looks like.
    ///
    /// # Why every one of these goes through here
    ///
    /// **A `fixed_size` on an `egui::Window` is a request, not a bound.** Put
    /// more in one than fits and egui grows it — so the settings page, which
    /// has a scrolling list of bindings and then volume sliders below it, ran
    /// off the top and the bottom of the screen with no way to reach either end.
    /// Reported from the window, and it is the sort of thing every screen would
    /// have got wrong separately.
    ///
    /// So the shape is decided here and the content is handed a `Ui` that is
    /// already inside it: a title bar with an optional Back, then whatever is
    /// left, scrolling. A screen cannot escape its own sheet because it never
    /// gets to say how big it is.
    ///
    /// Returns whether Back was pressed.
    pub fn sheet(
        ctx: &egui::Context,
        title: &str,
        back: Option<&str>,
        contents: impl FnOnce(&mut egui::Ui),
    ) -> bool {
        let screen = ctx.content_rect();
        let (width, height) = size((screen.width(), screen.height()));
        let (x, y) = origin((screen.width(), screen.height()));
        let mut went_back = false;
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .title_bar(false)
            .movable(false)
            .fixed_pos(egui::pos2(screen.left() + x, screen.top() + y))
            .fixed_size(egui::vec2(width, height))
            // Belt as well as braces: `fixed_size` is what egui aims for and
            // this is what it may not exceed, so a screen that asks for more
            // scrolls instead of growing.
            .max_height(height)
            .show(ctx, |ui| {
                ui.set_min_size(egui::vec2(width, height));
                // **The bar is the same on every screen**, which is the point:
                // a player who has learned where Back is has learned it once.
                ui.horizontal(|ui| {
                    if let Some(label) = back {
                        went_back |= ui.button(format!("← {label}")).clicked();
                        ui.separator();
                    }
                    ui.heading(title);
                });
                ui.separator();
                // Everything below scrolls. The header stays put, so the way
                // out is always on screen however long the page is.
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, contents);
            });
        went_back
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
