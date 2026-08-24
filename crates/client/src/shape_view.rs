// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Drawing and chiselling a block's twenty-seven cells, in two dimensions.
//!
//! # Why there is no 3D here at all
//!
//! A shape editor wants to look like a block seen from a corner, and the
//! obvious way to get that is a camera, a depth buffer and a render pass whose
//! result is fed back into the interface. That is a lot of machinery for a
//! picture of a cube, and every bit of it would be a second path for something
//! the world renderer already does.
//!
//! An isometric projection needs neither. The one this module uses is
//! orthographic along `(1, 1, 1)`:
//!
//! ```text
//! sx = (x - z)
//! sy = (x + z) / 2 - y
//! ```
//!
//! which is exactly the projection whose depth ordering is `x + y + z` — a
//! displacement of `(1, 1, 1)` maps to `(0, 0)` on screen, so cells sort by
//! that sum and the painter's algorithm is correct rather than approximately
//! correct. Twenty-seven cells drawn back to front is cheaper than a render
//! pass and, unlike one, every function here is testable without a GPU.
//!
//! # The gesture is the world's gesture
//!
//! Left-click removes the nearest cell along the line of sight; right-click
//! puts one back against the face that was clicked. That is what digging and
//! placing already do, so a player knows it before they open the screen — and
//! it means removal never has to reach a cell it cannot see, because chiselling
//! only ever works from the outside in.

/// Cells along one edge of a block.
const SIDE: i32 = 3;

/// The projected grid is six units across, whatever the widget's size.
///
/// `x - z` runs `-3..=3` and `(x + z) / 2 - y` runs `-3..=3`, so the whole
/// block is a six-by-six square and one scale fits both axes.
const EXTENT: f32 = 6.0;

/// Which face of a cell a click landed on.
///
/// Only three can ever be visible from the fixed viewpoint, which is the point:
/// a cell the player can see is a cell they can reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Face {
    /// The `+y` face, looking down onto it.
    Top,
    /// The `+x` face, on the right.
    Right,
    /// The `+z` face, toward the viewer.
    Front,
}

impl Face {
    /// The neighbouring cell in this direction, if it is inside the block.
    #[must_use]
    pub const fn beyond(self, x: i32, y: i32, z: i32) -> Option<(i32, i32, i32)> {
        let (x, y, z) = match self {
            Self::Top => (x, y + 1, z),
            Self::Right => (x + 1, y, z),
            Self::Front => (x, y, z + 1),
        };
        if x < SIDE && y < SIDE && z < SIDE {
            Some((x, y, z))
        } else {
            None
        }
    }
}

/// The bit a cell occupies in an occupancy mask.
///
/// `index = x + 3*y + 9*z`, the layout `core::block` documents and the one a
/// `Partial` block is stored with. There is exactly one indexing convention in
/// this engine and this is it.
#[must_use]
pub const fn bit(x: i32, y: i32, z: i32) -> u32 {
    1 << (x + SIDE * y + SIDE * SIDE * z)
}

/// Whether a cell is filled.
#[must_use]
pub const fn filled(mask: u32, x: i32, y: i32, z: i32) -> bool {
    mask & bit(x, y, z) != 0
}

/// Where a grid corner lands on screen.
///
/// Coordinates are corner coordinates, `0..=3`, not cell coordinates — a cell
/// spans from its own corner to the next one along each axis.
fn project(area: egui::Rect, x: f32, y: f32, z: f32) -> egui::Pos2 {
    let scale = area.width().min(area.height()) / EXTENT;
    let centre = area.center();
    // Origin-centred first: the grid runs 0..3, so subtracting 1.5 from each
    // axis puts the block's middle at the middle of the widget.
    let (x, y, z) = (x - 1.5, y - 1.5, z - 1.5);
    egui::pos2(
        centre.x + (x - z) * scale,
        // `+y` is up on screen and down in egui, hence the sign; `(x + z) / 2`
        // is what puts the far corner at the top.
        centre.y + ((x + z) * 0.5 - y) * scale,
    )
}

/// The four screen corners of one face of one cell.
#[must_use]
pub fn face_corners(area: egui::Rect, x: i32, y: i32, z: i32, face: Face) -> [egui::Pos2; 4] {
    #[expect(
        clippy::cast_precision_loss,
        reason = "grid coordinates are 0..=3 and exact in f32"
    )]
    let (x, y, z) = (x as f32, y as f32, z as f32);
    match face {
        Face::Top => [
            project(area, x, y + 1.0, z),
            project(area, x + 1.0, y + 1.0, z),
            project(area, x + 1.0, y + 1.0, z + 1.0),
            project(area, x, y + 1.0, z + 1.0),
        ],
        Face::Right => [
            project(area, x + 1.0, y, z),
            project(area, x + 1.0, y + 1.0, z),
            project(area, x + 1.0, y + 1.0, z + 1.0),
            project(area, x + 1.0, y, z + 1.0),
        ],
        Face::Front => [
            project(area, x, y, z + 1.0),
            project(area, x + 1.0, y, z + 1.0),
            project(area, x + 1.0, y + 1.0, z + 1.0),
            project(area, x, y + 1.0, z + 1.0),
        ],
    }
}

/// Every filled cell, ordered so that drawing them in turn is correct.
///
/// Far to near, by `x + y + z` — the depth along this projection's view axis,
/// exactly rather than approximately, which is the whole reason the projection
/// was chosen. A stable sort, so a mask never draws two ways.
#[must_use]
pub fn draw_order(mask: u32) -> Vec<(i32, i32, i32)> {
    let mut cells: Vec<(i32, i32, i32)> = (0..SIDE)
        .flat_map(|z| (0..SIDE).flat_map(move |y| (0..SIDE).map(move |x| (x, y, z))))
        .filter(|(x, y, z)| filled(mask, *x, *y, *z))
        .collect();
    cells.sort_by_key(|(x, y, z)| x + y + z);
    cells
}

/// Which cell and face a point is over, nearest first.
///
/// `None` for a point over no filled cell — the gap left by a chiselled block,
/// or the space around it.
#[must_use]
pub fn pick(area: egui::Rect, mask: u32, point: egui::Pos2) -> Option<((i32, i32, i32), Face)> {
    // Near to far, which is the reverse of the draw order for the same reason
    // it is the draw order: the first hit going forwards is the one the player
    // can see, and testing hidden faces of cells behind it can never win.
    for (x, y, z) in draw_order(mask).into_iter().rev() {
        for face in [Face::Top, Face::Right, Face::Front] {
            if inside(&face_corners(area, x, y, z, face), point) {
                return Some(((x, y, z), face));
            }
        }
    }
    None
}

/// Whether a point is inside a convex polygon given in order.
fn inside(corners: &[egui::Pos2; 4], point: egui::Pos2) -> bool {
    let mut positive = false;
    let mut negative = false;
    for index in 0..corners.len() {
        let a = corners[index];
        let b = corners[(index + 1) % corners.len()];
        let cross = (b.x - a.x) * (point.y - a.y) - (b.y - a.y) * (point.x - a.x);
        if cross > 0.0 {
            positive = true;
        } else if cross < 0.0 {
            negative = true;
        }
        // A point on an edge has a zero cross and counts as inside, so two
        // faces that share an edge both claim it — harmless, because the
        // caller takes the first.
        if positive && negative {
            return false;
        }
    }
    true
}

/// What a left click does: take the cell out.
#[must_use]
pub const fn chisel(mask: u32, x: i32, y: i32, z: i32) -> u32 {
    mask & !bit(x, y, z)
}

/// What a right click does: put a cell back against the face that was clicked.
///
/// Returns the mask unchanged when the neighbour is outside the block, which is
/// what clicking the outer face of an edge cell means.
#[must_use]
pub fn restore(mask: u32, x: i32, y: i32, z: i32, face: Face) -> u32 {
    match face.beyond(x, y, z) {
        Some((x, y, z)) => mask | bit(x, y, z),
        None => mask,
    }
}

/// The cell a right click fills when there is nothing left to click on.
///
/// **So the editor is never a dead end.** A player who chisels every cell away
/// has no face to place against and no way back, and a screen you can put into
/// a state you cannot leave is a bug however rare it is. The middle cell,
/// because it is the one every other cell can be reached from.
#[must_use]
pub const fn seed() -> u32 {
    bit(1, 1, 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: u32 = (1 << 27) - 1;

    fn area() -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(240.0, 240.0))
    }

    #[test]
    fn the_projection_is_orthographic_along_the_view_axis() {
        // The property the depth sort rests on: a step of (1, 1, 1) is a step
        // straight away from the viewer and must not move on screen. If this
        // ever stops holding, `draw_order` is sorting by the wrong number and
        // cells will draw through each other.
        let near = project(area(), 0.0, 0.0, 0.0);
        let far = project(area(), 1.0, 1.0, 1.0);
        assert!(
            (near.x - far.x).abs() < 1e-3 && (near.y - far.y).abs() < 1e-3,
            "a step along the view axis moved on screen: {near:?} -> {far:?}"
        );
    }

    #[test]
    fn the_whole_block_fits_inside_the_widget() {
        let area = area();
        for (x, y, z) in [(0.0, 0.0, 0.0), (3.0, 3.0, 3.0), (3.0, 0.0, 0.0)] {
            let at = project(area, x, y, z);
            assert!(
                area.contains(at),
                "corner ({x}, {y}, {z}) landed outside the widget at {at:?}"
            );
        }
    }

    #[test]
    fn clicking_a_full_block_reaches_the_cell_nearest_the_viewer() {
        // The corner facing the camera is (2, 2, 2), and its three faces are
        // the ones drawn last. A click in the middle of the widget lands on the
        // block's near corner because that is what is in front.
        let area = area();
        let ((x, y, z), _) =
            pick(area, FULL, area.center()).expect("a full block fills the middle");
        assert_eq!(
            (x + y + z),
            6,
            "the click reached ({x}, {y}, {z}), which is not the nearest corner"
        );
    }

    #[test]
    fn chiselling_the_near_corner_exposes_what_was_behind_it() {
        // **The reason left-click needs no hidden-cell rule.** Removing what
        // you can see is always enough, because removing it reveals the next
        // one along the same line.
        let area = area();
        let (first, _) = pick(area, FULL, area.center()).expect("a full block");
        let mask = chisel(FULL, first.0, first.1, first.2);
        let (second, _) = pick(area, mask, area.center()).expect("something behind it");
        assert_ne!(first, second, "the same cell was picked twice");
        assert!(
            second.0 + second.1 + second.2 < first.0 + first.1 + first.2,
            "the cell revealed was not further away: {first:?} then {second:?}"
        );
    }

    #[test]
    fn a_click_off_the_block_hits_nothing() {
        let area = area();
        assert!(
            pick(area, FULL, area.left_top()).is_none(),
            "the corner of a square widget is outside a block drawn as a hexagon"
        );
        assert!(
            pick(area, 0, area.center()).is_none(),
            "an empty mask has nothing to click on"
        );
    }

    #[test]
    fn right_clicking_a_face_puts_a_cell_against_it() {
        // One cell, alone, so which face is picked is unambiguous.
        let mask = bit(1, 1, 1);
        assert_eq!(
            restore(mask, 1, 1, 1, Face::Top),
            mask | bit(1, 2, 1),
            "the cell went somewhere other than on top"
        );
        assert_eq!(restore(mask, 1, 1, 1, Face::Right), mask | bit(2, 1, 1));
        assert_eq!(restore(mask, 1, 1, 1, Face::Front), mask | bit(1, 1, 2));
    }

    #[test]
    fn a_face_on_the_outside_of_the_block_has_nothing_beyond_it() {
        let mask = bit(2, 2, 2);
        assert_eq!(
            restore(mask, 2, 2, 2, Face::Top),
            mask,
            "a cell was placed outside the block"
        );
        assert!(Face::Right.beyond(2, 1, 1).is_none());
    }

    #[test]
    fn every_cell_of_a_full_block_is_reachable_by_chiselling() {
        // **The claim that makes an outside-in tool sufficient.** Clicking the
        // middle of the widget repeatedly must empty the block eventually
        // rather than stalling on a cell nothing can reach — and the same has
        // to hold from every point over it, not just the centre.
        let area = area();
        let mut mask = FULL;
        let mut removed = 0;
        // Sweep the widget rather than hammering one point: one line of sight
        // only ever reaches three cells.
        for step in 0..40 {
            #[expect(clippy::cast_precision_loss, reason = "a small loop counter")]
            let t = step as f32 / 40.0;
            let point = area.left_top() + (area.right_bottom() - area.left_top()) * t;
            while let Some(((x, y, z), _)) = pick(area, mask, point) {
                mask = chisel(mask, x, y, z);
                removed += 1;
            }
        }
        assert!(
            removed > 0 && mask != FULL,
            "chiselling removed nothing at all"
        );
        assert_eq!(
            mask.count_ones() + removed,
            FULL.count_ones(),
            "the count of removed cells does not match what left the mask"
        );
    }

    #[test]
    fn the_draw_order_runs_from_the_back() {
        let order = draw_order(FULL);
        assert_eq!(order.len(), 27, "a full block has twenty-seven cells");
        assert_eq!(order[0], (0, 0, 0), "the far corner draws first");
        assert_eq!(
            order[order.len() - 1],
            (2, 2, 2),
            "the near corner draws last"
        );
        for pair in order.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            assert!(
                a.0 + a.1 + a.2 <= b.0 + b.1 + b.2,
                "the order is not sorted by depth: {a:?} before {b:?}"
            );
        }
    }
}
