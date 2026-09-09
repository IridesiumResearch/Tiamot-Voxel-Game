// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Pictures a mod's interface draws: `Widget::Image`, and a style's nine-slice.
//!
//! # Why these are not the atlas
//!
//! A block texture goes into the world atlas, which is one big texture the
//! world shader samples with arithmetic rather than a lookup — every tile the
//! same size, scaled to fit. An interface picture is the opposite of all of
//! that: it is whatever size the artist drew, it is drawn by egui rather than
//! by the world shader, and a mod may push one at any moment rather than at
//! the join. So it gets its own store, and egui owns the texture.
//!
//! # Two stages, because they happen at different moments
//!
//! Bytes arrive on the network task and are decoded there, off the frame. A
//! texture can only be made where there is an `egui::Context`, which is inside
//! a frame. So a decoded picture waits here until something first draws it, and
//! is uploaded once.
//!
//! Charter rule 14: these are server-pushed assets and hostile until proven
//! otherwise. They go through the same guarded, panic-isolated decoder a block
//! texture does — see [`crate::texture::decode_or_missing`] — and arrive here
//! already decoded, so nothing in this file parses anything.

use std::collections::BTreeMap;

use tiamot_core::proto::ContentHash;

use crate::texture::Image;

/// Every interface picture this client has, decoded and possibly uploaded.
#[derive(Default)]
pub struct Pictures {
    decoded: BTreeMap<ContentHash, Image>,
    uploaded: BTreeMap<ContentHash, egui::TextureHandle>,
}

impl Pictures {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a decoded picture, replacing any it had by that hash.
    ///
    /// **The upload is dropped with it.** Content is addressed by hash, so the
    /// same hash is the same bytes and a replacement is only ever a re-fetch of
    /// something identical — but keeping a texture made from bytes that are no
    /// longer the ones in hand is the sort of thing that survives until the day
    /// hashes collide or a cache lies.
    pub fn insert(&mut self, hash: ContentHash, image: Image) {
        self.uploaded.remove(&hash);
        self.decoded.insert(hash, image);
    }

    /// Whether a picture by this hash has arrived.
    #[must_use]
    pub fn has(&self, hash: &ContentHash) -> bool {
        self.decoded.contains_key(hash)
    }

    /// How many pictures are held, for the debug overlay.
    #[must_use]
    pub fn len(&self) -> usize {
        self.decoded.len()
    }

    /// Whether nothing has arrived.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.decoded.is_empty()
    }

    /// The texture for a picture, uploading it the first time it is drawn.
    ///
    /// `None` for a picture that has not arrived — which is the ordinary state
    /// for the frame or two between a dialog opening and its art landing, and
    /// is why the painter draws nothing rather than a placeholder: a frame that
    /// flashes magenta on every dialog open is worse than one that fades in.
    pub fn texture(&mut self, ctx: &egui::Context, hash: &ContentHash) -> Option<egui::TextureId> {
        if let Some(handle) = self.uploaded.get(hash) {
            return Some(handle.id());
        }
        let image = self.decoded.get(hash)?;
        let size = [image.width as usize, image.height as usize];
        let colour = egui::ColorImage::from_rgba_unmultiplied(size, &image.rgba);
        // **Linear, not nearest.** A block texture is nearest-sampled because a
        // voxel world wants its pixels crisp at any distance; interface art is
        // drawn at whatever size the layout gives it, and a panel scaled 1.3x
        // with nearest sampling has visibly uneven edges.
        let handle = ctx.load_texture(
            format!("mod-picture-{}", crate::trust::to_hex(hash)),
            colour,
            egui::TextureOptions::LINEAR,
        );
        let id = handle.id();
        self.uploaded.insert(*hash, handle);
        Some(id)
    }

    /// Uploads every picture a tree draws and hands back what to paint with.
    ///
    /// **Resolved before the paint walk rather than during it.** Uploading
    /// needs `&mut self` and the walk is a recursion carrying `&mut egui::Ui`;
    /// threading a second mutable borrow through it would mean restructuring
    /// the recursion around a detail of texture upload. A tree names a handful
    /// of pictures, so doing them all first costs a map of two or three
    /// entries and keeps the walk immutable.
    pub fn resolve(&mut self, ctx: &egui::Context, tree: &tiamot_core::ui::Tree) -> Resolved {
        self.resolve_hashes(ctx, &tree.content())
    }

    /// The same, for pictures named one at a time rather than by a tree.
    ///
    /// What a HUD script's frame gives: a list of draw commands, some of which
    /// name a picture. There is no tree to ask.
    pub fn resolve_hashes(&mut self, ctx: &egui::Context, hashes: &[ContentHash]) -> Resolved {
        let mut out = Resolved::default();
        for hash in hashes.iter().copied() {
            // The size is copied out before `texture` borrows the store again.
            let Some(&Image { width, height, .. }) = self.decoded.get(&hash) else {
                continue;
            };
            if let Some(texture) = self.texture(ctx, &hash) {
                out.0.insert(
                    hash,
                    Picture {
                        texture,
                        width,
                        height,
                    },
                );
            }
        }
        out
    }

    /// Forgets everything, for a client leaving a world.
    ///
    /// A picture belongs to the server that pushed it: carrying one into the
    /// next world would be a mod's art appearing in somebody else's game.
    pub fn clear(&mut self) {
        self.decoded.clear();
        self.uploaded.clear();
    }
}

/// One picture, ready to draw.
#[derive(Debug, Clone, Copy)]
pub struct Picture {
    /// What egui draws it with.
    pub texture: egui::TextureId,
    /// The source width, which a nine-slice needs to know where its cuts are.
    pub width: u32,
    /// The source height.
    pub height: u32,
}

/// Every picture one tree draws, by hash.
///
/// Immutable by the time a painter sees it, which is the point — see
/// [`Pictures::resolve`].
#[derive(Debug, Default)]
pub struct Resolved(BTreeMap<ContentHash, Picture>);

impl Resolved {
    /// The picture for a hash, or `None` if it has not arrived.
    #[must_use]
    pub fn get(&self, hash: &ContentHash) -> Option<Picture> {
        self.0.get(hash).copied()
    }

    /// Whether nothing in this tree has art yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Paints a picture stretched over a rectangle.
///
/// The whole image, with no aspect correction: a mod asked for a picture in a
/// box it chose, and quietly letterboxing it would be the engine having an
/// opinion about somebody's art.
pub fn paint(painter: &egui::Painter, texture: egui::TextureId, rect: egui::Rect) {
    painter.image(
        texture,
        rect,
        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        egui::Color32::WHITE,
    );
}

/// Paints a nine-slice frame: corners unstretched, edges and centre stretched.
///
/// # The border is a THIRD of the image
///
/// The wire carries a hash and nothing else — no border widths — so the
/// engine has to say where the cuts are, and thirds is the rule a mod can draw
/// to without being told a number. An image 48 pixels across has 16-pixel
/// corners; one 96 across has 32-pixel corners.
///
/// # Why the corners keep their size
///
/// That is the whole point of a nine-slice. A panel stretched as one image gets
/// fat rounded corners on a wide dialog and thin ones on a narrow dialog, and a
/// frame that changes shape with its contents reads as a bug in the art. Here
/// the four corners are drawn at their own pixel size, the edges stretch along
/// one axis only, and the middle takes what is left.
///
/// A rectangle smaller than two corners would have the corners overlap; the
/// corner size is clamped to half the box so a frame around a small button
/// degrades to four corners meeting rather than to art drawn inside out.
pub fn paint_nine_slice(
    painter: &egui::Painter,
    texture: egui::TextureId,
    rect: egui::Rect,
    scale: f32,
    source: (u32, u32),
) {
    let (width, height) = source;
    let third = egui::vec2(width as f32 / 3.0, height as f32 / 3.0) * scale;
    // Never more than half the box, or opposite corners would overlap and each
    // would be drawn twice — visible immediately with any transparency.
    let corner = egui::vec2(
        third.x.min(rect.width() / 2.0),
        third.y.min(rect.height() / 2.0),
    );

    // Both grids are the same shape: three columns and three rows, one in the
    // box and one in the image. Written once so a mismatch is impossible.
    let xs = [
        rect.min.x,
        rect.min.x + corner.x,
        rect.max.x - corner.x,
        rect.max.x,
    ];
    let ys = [
        rect.min.y,
        rect.min.y + corner.y,
        rect.max.y - corner.y,
        rect.max.y,
    ];
    let us = [0.0, 1.0 / 3.0, 2.0 / 3.0, 1.0];

    for row in 0..3 {
        for column in 0..3 {
            let piece = egui::Rect::from_min_max(
                egui::pos2(xs[column], ys[row]),
                egui::pos2(xs[column + 1], ys[row + 1]),
            );
            if piece.width() <= 0.0 || piece.height() <= 0.0 {
                continue;
            }
            let uv = egui::Rect::from_min_max(
                egui::pos2(us[column], us[row]),
                egui::pos2(us[column + 1], us[row + 1]),
            );
            painter.image(texture, piece, uv, egui::Color32::WHITE);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(byte: u8) -> ContentHash {
        [byte; 32]
    }

    #[test]
    fn a_picture_is_held_until_something_draws_it() {
        // The two stages: bytes are decoded on the network task, and a texture
        // can only be made inside a frame. A store that uploaded on arrival
        // would need an `egui::Context` on the network task, which is where
        // this design would go wrong.
        let mut pictures = Pictures::new();
        assert!(!pictures.has(&hash(1)));
        pictures.insert(hash(1), Image::solid(4, 4, [255, 0, 0, 255]));
        assert!(pictures.has(&hash(1)));
        assert_eq!(pictures.len(), 1);
    }

    #[test]
    fn replacing_a_picture_drops_the_texture_made_from_the_old_one() {
        // Content is addressed by hash, so this should never differ in
        // practice — which is exactly why it is worth being right about.
        let mut pictures = Pictures::new();
        pictures.insert(hash(2), Image::solid(2, 2, [1, 2, 3, 4]));
        pictures.uploaded.insert(
            hash(2),
            egui::Context::default().load_texture(
                "test",
                egui::ColorImage::new([1, 1], vec![egui::Color32::WHITE]),
                egui::TextureOptions::LINEAR,
            ),
        );
        pictures.insert(hash(2), Image::solid(2, 2, [9, 9, 9, 9]));
        assert!(
            !pictures.uploaded.contains_key(&hash(2)),
            "the texture made from the old bytes survived them"
        );
    }

    /// Runs a paint closure and hands back every shape it produced.
    fn painted(draw: impl Fn(&egui::Painter)) -> Vec<egui::epaint::ClippedShape> {
        let ctx = egui::Context::default();
        let output = ctx.run_ui(egui::RawInput::default(), |root| {
            let painter = root.ctx().layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("test"),
            ));
            draw(&painter);
        });
        output.shapes
    }

    /// The rectangles of every textured mesh in a set of shapes.
    fn meshes(shapes: &[egui::epaint::ClippedShape]) -> Vec<egui::Rect> {
        shapes
            .iter()
            .filter_map(|clipped| match &clipped.shape {
                egui::Shape::Mesh(mesh) => Some(mesh.calc_bounds()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_nine_slice_keeps_its_corners_and_stretches_the_rest() {
        // **The whole reason a nine-slice exists.** A panel stretched as one
        // image has fat corners when it is wide and thin ones when it is
        // narrow, and a frame that changes shape with its contents reads as
        // broken art. So: nine pieces, and the four corners are the source's
        // own thirds however big the box is.
        let texture = egui::TextureId::Managed(1);
        let wide = painted(|painter| {
            paint_nine_slice(
                painter,
                texture,
                egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(300.0, 90.0)),
                1.0,
                (48, 48),
            );
        });
        let narrow = painted(|painter| {
            paint_nine_slice(
                painter,
                texture,
                egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(90.0, 90.0)),
                1.0,
                (48, 48),
            );
        });

        let wide = meshes(&wide);
        let narrow = meshes(&narrow);
        assert_eq!(wide.len(), 9, "a nine-slice draws nine pieces");
        assert_eq!(narrow.len(), 9);

        // The top-left corner is 16x16 — a third of 48 — in both, though one
        // box is more than three times the width of the other.
        let corner = |pieces: &[egui::Rect]| {
            pieces
                .iter()
                .find(|rect| rect.min.x < 1.0 && rect.min.y < 1.0)
                .copied()
                .expect("a top-left piece")
        };
        assert!(
            (corner(&wide).width() - 16.0).abs() < 0.01,
            "the wide box's corner is {}, not 16",
            corner(&wide).width()
        );
        assert!(
            (corner(&narrow).width() - 16.0).abs() < 0.01,
            "the narrow box's corner is {}, not 16",
            corner(&narrow).width()
        );
    }

    #[test]
    fn a_nine_slice_in_a_box_smaller_than_its_corners_does_not_overlap_them() {
        // A frame around a small button. Without the clamp the left corner
        // reaches past the right one, every piece is drawn inside out, and with
        // any transparency the overlap is immediately visible.
        let shapes = painted(|painter| {
            paint_nine_slice(
                painter,
                egui::TextureId::Managed(1),
                egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(20.0, 20.0)),
                1.0,
                (48, 48),
            );
        });
        for rect in meshes(&shapes) {
            assert!(
                rect.width() >= 0.0 && rect.height() >= 0.0,
                "a piece came out inside out: {rect:?}"
            );
            assert!(
                rect.max.x <= 20.01 && rect.max.y <= 20.01,
                "a piece reached outside the box: {rect:?}"
            );
        }
    }

    #[test]
    fn leaving_a_world_forgets_its_art() {
        // A picture belongs to the server that pushed it.
        let mut pictures = Pictures::new();
        pictures.insert(hash(3), Image::solid(1, 1, [0; 4]));
        pictures.clear();
        assert!(pictures.is_empty());
    }
}
