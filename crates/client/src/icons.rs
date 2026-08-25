// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Drawing a material in the interface, from the texture the world uses.
//!
//! # One atlas, two consumers
//!
//! The world pass samples the atlas through the renderer's bind group. The
//! interface — inventory slots, the hotbar a mod draws with the tier-2 `Icon`
//! command — has to show the same pixels, or a slot and the wall built from it
//! disagree about what stone looks like. That is the sort of difference a
//! player notices and nobody can explain.
//!
//! So there is exactly one atlas texture. egui is handed a *view* of it
//! ([`egui_wgpu::Renderer::register_native_texture`]) and the layout to point
//! into it with ([`TileMap`]); neither is a second copy of the image.
//!
//! # Both halves can be missing, and separately
//!
//! The material table arrives from the server after the window exists, so on
//! the first frames there is no atlas at all — and a slot still has to draw
//! something. [`Icons::paint`] falls back to [`crate::dialog::material_tint`],
//! which is a hash of the material id: distinguishable, stable, and obviously
//! not a texture.

use crate::texture::TileMap;

/// The atlas, as the interface sees it.
///
/// Borrowed rather than owned because its two halves live in different places:
/// the texture id belongs to whoever owns the egui renderer, and the layout
/// arrives with the material table. Built at the call site each frame; it is
/// two words and a pointer.
#[derive(Clone, Copy, Default)]
pub struct Icons<'a> {
    texture: Option<egui::TextureId>,
    tiles: Option<&'a TileMap>,
}

impl<'a> Icons<'a> {
    /// The bridge, from an egui texture id and the atlas layout.
    #[must_use]
    pub const fn new(texture: Option<egui::TextureId>, tiles: Option<&'a TileMap>) -> Self {
        Self { texture, tiles }
    }

    /// Where a material is, if the atlas is up.
    ///
    /// Both halves are required: an id with no layout would sample tile zero
    /// for every material, and a layout with no id has nothing to sample.
    #[must_use]
    pub fn of(&self, material: u16) -> Option<(egui::TextureId, egui::Rect)> {
        let (texture, tiles) = (self.texture?, self.tiles?);
        let (u0, v0, u1, v1) = tiles.uv_of(material)?;
        Some((
            texture,
            egui::Rect::from_min_max(egui::pos2(u0, v0), egui::pos2(u1, v1)),
        ))
    }

    /// Draws a material into `rect`, textured if it can be and tinted if not.
    ///
    /// The single place the fallback is decided, so every material shown
    /// anywhere in the interface degrades the same way on the frames before
    /// the atlas exists.
    pub fn paint(&self, painter: &egui::Painter, rect: egui::Rect, material: u16) {
        if let Some((texture, uv)) = self.of(material) {
            painter.image(texture, rect, uv, egui::Color32::WHITE);
        } else {
            painter.rect_filled(rect, 2.0, crate::dialog::material_tint(material));
        }
    }

    /// Draws what a stack looks like: a cut as its cells, loose material as its
    /// tile.
    ///
    /// # Why a cut cannot be drawn as a tile
    ///
    /// **A shape is the only thing that tells two stacks of one material
    /// apart.** They stack separately, they cost different amounts and they
    /// place differently, and an interface that drew the material's tile for
    /// both showed a player a block of stone where their stairs were. Reported
    /// from the window, of a cut that had just been made.
    ///
    /// The cells are the same projection the shape editor uses — see
    /// [`crate::shape_view`] — so a cut looks the same wherever it is shown:
    /// in the editor that made it, in a slot, and on the hotbar.
    pub fn paint_stack(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        material: u16,
        shape: u32,
    ) {
        if shape == 0 || shape == tiamot_core::inventory::Shape::ALL {
            self.paint(painter, rect, material);
            return;
        }
        self.paint_cells(painter, rect, material, shape);
    }

    /// Draws a mask's cells, whatever the mask is.
    ///
    /// Separate from [`Icons::paint_stack`] because the shape EDITOR starts
    /// from a whole block and has to draw it as twenty-seven cells — that is
    /// the thing being chiselled. A whole block in a SLOT is loose material and
    /// draws as its tile.
    pub fn paint_cells(&self, painter: &egui::Painter, rect: egui::Rect, material: u16, mask: u32) {
        // Square and centred: the projection fits a six-by-six box, and
        // stretching it would put the cells' faces out of true with each other.
        let side = rect.width().min(rect.height());
        let area = egui::Rect::from_center_size(rect.center(), egui::vec2(side, side));
        for (x, y, z) in crate::shape_view::draw_order(mask) {
            for face in [
                crate::shape_view::Face::Front,
                crate::shape_view::Face::Right,
                crate::shape_view::Face::Top,
            ] {
                let corners = crate::shape_view::face_corners(area, x, y, z, face);
                crate::dialog::paint_cell_face(painter, corners, *self, material, face);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::texture::Atlas;

    /// Two materials, so the atlas has a grid worth resolving.
    fn atlas() -> Atlas {
        Atlas::build(&[None, None, None, None])
    }

    #[test]
    fn a_material_maps_to_its_own_corner_of_the_atlas() {
        let tiles = atlas().tiles_only();
        let icons = Icons::new(Some(egui::TextureId::User(7)), Some(&tiles));
        let (_, first) = icons.of(0).expect("the atlas is up");
        let (_, third) = icons.of(2).expect("the atlas is up");
        assert_ne!(
            first, third,
            "two materials that share a rectangle would draw as the same block"
        );
        assert!(
            first.min.x >= 0.0 && first.max.x <= 1.0 && first.max.y <= 1.0,
            "a tile outside 0..1 samples off the atlas: {first:?}"
        );
    }

    #[test]
    fn the_tile_excludes_the_padding_that_stops_mips_bleeding() {
        let tiles = atlas().tiles_only();
        let icons = Icons::new(Some(egui::TextureId::User(1)), Some(&tiles));
        let (_, uv) = icons.of(0).expect("the atlas is up");
        assert!(
            uv.min.x > 0.0,
            "starting at the atlas edge would draw the padding, not the tile"
        );
    }

    #[test]
    fn half_a_bridge_draws_nothing() {
        let tiles = atlas().tiles_only();
        assert!(
            Icons::new(None, Some(&tiles)).of(0).is_none(),
            "a layout with no texture has nothing to sample"
        );
        assert!(
            Icons::new(Some(egui::TextureId::User(1)), None)
                .of(0)
                .is_none(),
            "a texture with no layout would show every material as tile zero"
        );
        assert!(
            Icons::default().of(0).is_none(),
            "the frames before the material table arrives have no atlas"
        );
    }

    /// Everything one call to [`Icons::paint`] actually put on the screen.
    fn painted(icons: Icons<'_>) -> Vec<egui::epaint::Primitive> {
        let ctx = egui::Context::default();
        let output = ctx.run_ui(egui::RawInput::default(), |root| {
            let painter = root.ctx().layer_painter(egui::LayerId::background());
            icons.paint(
                &painter,
                egui::Rect::from_min_size(egui::pos2(4.0, 4.0), egui::vec2(32.0, 32.0)),
                1,
            );
        });
        ctx.tessellate(output.shapes, 1.0)
            .into_iter()
            .map(|clipped| clipped.primitive)
            .collect()
    }

    #[test]
    fn a_material_with_an_atlas_is_drawn_from_the_atlas() {
        let tiles = atlas().tiles_only();
        let id = egui::TextureId::User(11);
        let textured = painted(Icons::new(Some(id), Some(&tiles)));
        assert!(
            textured.iter().any(|primitive| matches!(
                primitive,
                egui::epaint::Primitive::Mesh(mesh) if mesh.texture_id == id
            )),
            "a slot with an atlas must sample it, not fall back to a tint"
        );

        // The counter-example, so the assertion above is visibly not vacuous:
        // with no atlas the same call draws with egui's own font texture,
        // which is what a flat rectangle uses.
        let tinted = painted(Icons::default());
        assert!(
            tinted.iter().all(|primitive| !matches!(
                primitive,
                egui::epaint::Primitive::Mesh(mesh) if mesh.texture_id == id
            )),
            "there is no atlas to sample, so nothing may claim to have sampled one"
        );
        assert!(
            !tinted.is_empty(),
            "the fallback still has to draw something the player can see"
        );
    }

    #[test]
    fn an_unknown_material_still_gets_a_rectangle() {
        let tiles = atlas().tiles_only();
        let icons = Icons::new(Some(egui::TextureId::User(1)), Some(&tiles));
        assert!(
            icons.of(u16::MAX).is_some(),
            "an id past the table falls back to the placeholder tile, not to nothing"
        );
    }
}
