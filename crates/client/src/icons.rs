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
    /// Materials that may not be placed: the items.
    ///
    /// **Because an item is not drawn like a block.** A block is a cube seen
    /// from a corner and a sword is not — it is a picture of a sword, and
    /// wrapping that picture around three faces makes three swords at three
    /// angles. Reported from the window, of exactly that.
    items: Option<&'a std::collections::BTreeSet<u16>>,
}

impl<'a> Icons<'a> {
    /// The bridge, from an egui texture id and the atlas layout.
    #[must_use]
    pub const fn new(texture: Option<egui::TextureId>, tiles: Option<&'a TileMap>) -> Self {
        Self {
            texture,
            tiles,
            items: None,
        }
    }

    /// The same, told which materials are items.
    ///
    /// Separate from [`Icons::new`] rather than a fourth argument everywhere: a
    /// caller with no material table has no items either, and the frames before
    /// one arrives are exactly when that is true.
    #[must_use]
    pub const fn with_items(mut self, items: &'a std::collections::BTreeSet<u16>) -> Self {
        self.items = Some(items);
        self
    }

    /// Whether this material is an item rather than a block.
    #[must_use]
    pub fn is_item(&self, material: u16) -> bool {
        self.items.is_some_and(|items| items.contains(&material))
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
        // **An item is a picture, not a solid.** A sword drawn as a cube is a
        // sword wrapped around three faces at three angles, which is what a
        // player sees and cannot unsee. Flat, filling the slot, the way every
        // game that has both draws them.
        if self.is_item(material) {
            self.paint_flat(painter, rect, material);
            return;
        }
        if shape == 0 || shape == tiamot_core::inventory::Shape::ALL {
            self.paint_block(painter, rect, material);
            return;
        }
        self.paint_cells(painter, rect, material, shape);
    }

    /// Draws a material as a flat picture filling `rect`.
    ///
    /// One quad, textured if the atlas is up and tinted if it is not — the same
    /// fallback everything else here takes, so an item degrades like a block on
    /// the frames before the material table arrives.
    pub fn paint_flat(&self, painter: &egui::Painter, rect: egui::Rect, material: u16) {
        if let Some((texture, uv)) = self.of(material) {
            painter.image(texture, rect, uv, egui::Color32::WHITE);
        } else {
            painter.rect_filled(rect, 2.0, crate::dialog::material_tint(material));
        }
    }

    /// Draws a whole block as a cube seen from a corner.
    ///
    /// # Why a slot is not a flat square
    ///
    /// **A flat tile is one face of a block, and a player reads it as a
    /// sticker.** Reported from the window: the inventory and the hotbar
    /// "display as a square for the most part", and an angled block is what
    /// they should be. Three faces at three brightnesses is what says the thing
    /// in the slot is a solid object, and it is the same projection a cut is
    /// drawn in — so a block and a stair cut from it look like the same
    /// material seen the same way.
    ///
    /// Three quads rather than the twenty-seven cells of a full mask: the cube
    /// is identical and the seams between cell edges are not.
    pub fn paint_block(&self, painter: &egui::Painter, rect: egui::Rect, material: u16) {
        let area = square(rect);
        for face in [
            crate::shape_view::Face::Front,
            crate::shape_view::Face::Right,
            crate::shape_view::Face::Top,
        ] {
            let corners = crate::shape_view::block_corners(area, face);
            crate::dialog::paint_cell_face(painter, corners, *self, material, face);
        }
    }

    /// Draws a mask's cells, whatever the mask is.
    ///
    /// Separate from [`Icons::paint_stack`] because the shape EDITOR starts
    /// from a whole block and has to draw it as twenty-seven cells — that is
    /// the thing being chiselled. A whole block in a SLOT is loose material and
    /// draws as its tile.
    pub fn paint_cells(&self, painter: &egui::Painter, rect: egui::Rect, material: u16, mask: u32) {
        let area = square(rect);
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

/// The largest centred square inside `rect`.
///
/// The projection fits a square box, and stretching it would put the cube's
/// faces out of true with each other.
fn square(rect: egui::Rect) -> egui::Rect {
    let side = rect.width().min(rect.height());
    egui::Rect::from_center_size(rect.center(), egui::vec2(side, side))
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

    /// Everything one call to [`Icons::paint_stack`] put on the screen.
    fn painted_stack(icons: Icons<'_>, shape: u32) -> Vec<egui::epaint::Primitive> {
        let ctx = egui::Context::default();
        let output = ctx.run_ui(egui::RawInput::default(), |root| {
            let painter = root.ctx().layer_painter(egui::LayerId::background());
            icons.paint_stack(
                &painter,
                egui::Rect::from_min_size(egui::pos2(4.0, 4.0), egui::vec2(32.0, 32.0)),
                1,
                shape,
            );
        });
        ctx.tessellate(output.shapes, 1.0)
            .into_iter()
            .map(|clipped| clipped.primitive)
            .collect()
    }

    #[test]
    fn a_whole_block_in_a_slot_is_a_cube_and_not_a_square() {
        // **Reported from the window**: a slot drew one face of the atlas tile,
        // which reads as a sticker rather than as a solid thing. Three faces at
        // three brightnesses is what says it is a block.
        //
        // Counted in VERTICES rather than by eye: a flat tile is one quad and a
        // cube seen from a corner is three, and no arrangement of one quad
        // makes twelve corners.
        let tiles = atlas().tiles_only();
        let icons = Icons::new(Some(egui::TextureId::User(3)), Some(&tiles));
        let corners = |drawn: Vec<egui::epaint::Primitive>| {
            drawn
                .iter()
                .map(|primitive| match primitive {
                    egui::epaint::Primitive::Mesh(mesh) => mesh.vertices.len(),
                    egui::epaint::Primitive::Callback(_) => 0,
                })
                .sum::<usize>()
        };
        let block = corners(painted_stack(icons, 0));
        assert!(
            block >= 12,
            "a whole block drew {block} vertices, which is not three faces"
        );

        // And a cut is still its own cells, which is more of them again.
        let cut = corners(painted_stack(icons, 0b111 << 12));
        assert!(
            cut > block,
            "a three-cell cut drew {cut} vertices and a whole block drew {block}"
        );
    }

    #[test]
    fn an_item_is_a_flat_picture_and_a_block_is_a_cube() {
        // **Reported from the window**: the sword appeared "placed on a block,
        // three of them at different angles". It was — a whole material was
        // drawn as a cube seen from a corner, and wrapping a picture of a sword
        // round three faces makes three swords.
        //
        // Counted in vertices, as the cube test beside this one is: a flat
        // picture is one quad and a cube is three, and no arrangement of one
        // quad makes twelve corners.
        let tiles = atlas().tiles_only();
        let items: std::collections::BTreeSet<u16> = [1u16].into_iter().collect();
        let icons = Icons::new(Some(egui::TextureId::User(3)), Some(&tiles)).with_items(&items);
        let corners = |drawn: Vec<egui::epaint::Primitive>| {
            drawn
                .iter()
                .map(|primitive| match primitive {
                    egui::epaint::Primitive::Mesh(mesh) => mesh.vertices.len(),
                    egui::epaint::Primitive::Callback(_) => 0,
                })
                .sum::<usize>()
        };

        // Material 1 is in the item set, so it is a picture.
        let item = corners(painted_stack(icons, 0));
        assert!(
            item <= 4,
            "an item drew {item} vertices, which is more than one quad"
        );

        // The counter-example, and it is the whole test: the SAME call with the
        // same material, told only that it is not an item, is a cube.
        let empty = std::collections::BTreeSet::new();
        let blocks = Icons::new(Some(egui::TextureId::User(3)), Some(&tiles)).with_items(&empty);
        let block = corners(painted_stack(blocks, 0));
        assert!(
            block >= 12,
            "a block drew {block} vertices, so this test is not comparing two shapes"
        );
    }

    #[test]
    fn a_material_with_an_atlas_is_drawn_from_the_atlas() {
        let tiles = atlas().tiles_only();
        let id = egui::TextureId::User(11);
        let textured = painted_stack(Icons::new(Some(id), Some(&tiles)), 0);
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
        let tinted = painted_stack(Icons::default(), 0);
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
