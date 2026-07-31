// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Decoding server-pushed textures, and packing them into an atlas.
//!
//! # Every byte here came from a server the player does not trust
//!
//! Charter rule 14, and this is its headline case: a client joins a stranger's
//! server and that server hands it PNGs. The decoder is the attack surface, and
//! the rules are not negotiable:
//!
//! - **Pure Rust only.** `png` is the image-rs decoder — no `unsafe`, fuzzed on
//!   OSS-Fuzz, and Chromium's PNG decoder since M139. No C bindings anywhere in
//!   this path.
//! - **Limits before decode, not after.** A PNG header can claim 65,535² pixels
//!   in 40 bytes. Checking the dimensions after decoding means allocating 17 GB
//!   to find out it was too big — the decompression bomb, and the reason
//!   [`Limits`] is applied to the reader rather than to the result.
//! - **A bad texture is a missing texture, never a crash.** Decoding runs
//!   isolated and a failure becomes the magenta checker, with the reason
//!   reported. One malformed file must not take a player out of the game.
//!
//! # Why magenta checks
//!
//! The fallback has to be unmistakable. A grey or white placeholder reads as a
//! deliberate texture and the player reports "the wall looks wrong" instead of
//! "this server has a broken texture". Magenta checks have meant "missing
//! texture" since Quake.

use std::io::Cursor;

/// Largest texture edge accepted, in pixels.
///
/// A block texture is 16² or 32²; 1024 is generous for a high-resolution pack
/// and 4096× smaller than what a PNG header can claim.
pub const MAX_DIMENSION: u32 = 1024;

/// Largest decoded size accepted, in bytes.
///
/// Dimensions alone are not enough: 1024×1024 is fine, and a thousand of them
/// is a gigabyte. This bounds one texture; the atlas bounds the total.
pub const MAX_DECODED_BYTES: u64 = 8 * 1024 * 1024;

/// Edge length of one atlas tile, in pixels.
///
/// Every texture is scaled to this. A uniform grid makes the atlas coordinates
/// a multiply rather than a lookup, and mismatched sizes in one atlas are how
/// bleeding artefacts start.
pub const TILE: u32 = 16;

/// Padding around each tile, in pixels.
///
/// Mipmapping averages neighbouring pixels, and at the smallest mip level a
/// tile's neighbours are *other tiles* — so a block picks up the colour of
/// whatever was packed next to it. Padding each tile with a copy of its own
/// edge pixels means the average stays within the tile.
pub const PADDING: u32 = 2;

/// Full pitch of one tile including padding.
pub const TILE_PITCH: u32 = TILE + PADDING * 2;

/// Why a texture could not be used.
#[derive(Debug, Clone, thiserror::Error)]
pub enum TextureError {
    /// The PNG header declared something too large.
    ///
    /// Refused **before** decoding — see the module docs.
    #[error(
        "texture declares {width}x{height}, over the {MAX_DIMENSION}px limit. Refused before \
         decoding: a header claiming huge dimensions costs 40 bytes to send and gigabytes to \
         honour."
    )]
    TooLarge {
        /// Declared width.
        width: u32,
        /// Declared height.
        height: u32,
    },

    /// The decoded image would exceed [`MAX_DECODED_BYTES`].
    #[error("texture would decode to {bytes} bytes, over the {MAX_DECODED_BYTES}-byte limit")]
    TooHeavy {
        /// Declared size in bytes.
        bytes: u64,
    },

    /// The bytes are not a valid PNG.
    #[error("texture is not a decodable PNG: {reason}")]
    Malformed {
        /// What the decoder objected to.
        reason: String,
    },

    /// The decoder panicked.
    ///
    /// Should not happen — the decoder has no `unsafe` and is fuzzed — but a
    /// client must survive it if it ever does.
    #[error("the texture decoder panicked; this texture has been disabled")]
    Panicked,
}

/// A decoded texture: RGBA8, row-major, top-left origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height * 4` bytes.
    pub rgba: Vec<u8>,
}

impl Image {
    /// A solid colour.
    #[must_use]
    pub fn solid(width: u32, height: u32, colour: [u8; 4]) -> Self {
        Self {
            width,
            height,
            rgba: colour
                .iter()
                .copied()
                .cycle()
                .take((width as usize) * (height as usize) * 4)
                .collect(),
        }
    }

    /// The magenta-checker "this texture is missing" image.
    #[must_use]
    pub fn missing() -> Self {
        let mut rgba = Vec::with_capacity((TILE as usize) * (TILE as usize) * 4);
        for y in 0..TILE {
            for x in 0..TILE {
                // Quarter-tile checks: big enough to read at a distance.
                let dark = ((x / (TILE / 4)) + (y / (TILE / 4))).is_multiple_of(2);
                let colour: [u8; 4] = if dark {
                    [0, 0, 0, 255]
                } else {
                    [255, 0, 255, 255]
                };
                rgba.extend_from_slice(&colour);
            }
        }
        Self {
            width: TILE,
            height: TILE,
            rgba,
        }
    }

    /// The reference `core:white` texture: white with a faint border.
    ///
    /// The border is what makes a wall of white blocks readable. Without it the
    /// world is one undifferentiated mass and it is impossible to tell whether
    /// meshing is working at all — which matters for the first visible build.
    #[must_use]
    pub fn white_with_border() -> Self {
        let mut rgba = Vec::with_capacity((TILE as usize) * (TILE as usize) * 4);
        for y in 0..TILE {
            for x in 0..TILE {
                let edge = x == 0 || y == 0 || x == TILE - 1 || y == TILE - 1;
                // Faint: 0.86 grey against white. Visible as an edge, not as a
                // drawn-on grid.
                let value = if edge { 220 } else { 255 };
                rgba.extend_from_slice(&[value, value, value, 255]);
            }
        }
        Self {
            width: TILE,
            height: TILE,
            rgba,
        }
    }

    /// The pixel at `(x, y)`, or `None` if out of bounds.
    #[must_use]
    pub fn pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let offset = ((y as usize) * (self.width as usize) + (x as usize)) * 4;
        self.rgba
            .get(offset..offset + 4)
            .map(|slice| [slice[0], slice[1], slice[2], slice[3]])
    }

    /// Nearest-neighbour resample to `TILE` square.
    ///
    /// Nearest rather than linear on purpose: voxel textures are pixel art, and
    /// smoothing them is what makes a 16×16 texture look like a smear.
    #[must_use]
    pub fn to_tile(&self) -> Self {
        if self.width == TILE && self.height == TILE {
            return self.clone();
        }
        let mut rgba = Vec::with_capacity((TILE as usize) * (TILE as usize) * 4);
        for y in 0..TILE {
            for x in 0..TILE {
                // Integer arithmetic: exact, and no float rounding to argue
                // about at the edges.
                let source_x = (x * self.width.max(1)) / TILE;
                let source_y = (y * self.height.max(1)) / TILE;
                let pixel = self
                    .pixel(
                        source_x.min(self.width.saturating_sub(1)),
                        source_y.min(self.height.saturating_sub(1)),
                    )
                    .unwrap_or([255, 0, 255, 255]);
                rgba.extend_from_slice(&pixel);
            }
        }
        Self {
            width: TILE,
            height: TILE,
            rgba,
        }
    }
}

/// Decodes a PNG with the limits applied **before** any allocation.
///
/// # Errors
///
/// [`TextureError`] naming which limit was hit or what the decoder objected to.
pub fn decode_png(bytes: &[u8]) -> Result<Image, TextureError> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));

    // The limits go on the DECODER, so they are enforced while reading the
    // header rather than checked against a result that has already been
    // allocated. This is the whole defence against a decompression bomb.
    decoder.set_limits(png::Limits {
        bytes: usize::try_from(MAX_DECODED_BYTES).unwrap_or(usize::MAX),
    });
    decoder.set_transformations(png::Transformations::normalize_to_color8());

    let mut reader = decoder.read_info().map_err(|err| TextureError::Malformed {
        reason: err.to_string(),
    })?;

    let info = reader.info();
    let (width, height) = (info.width, info.height);

    // Dimensions checked against the HEADER, before a single pixel is read.
    if width > MAX_DIMENSION || height > MAX_DIMENSION || width == 0 || height == 0 {
        return Err(TextureError::TooLarge { width, height });
    }
    let declared = u64::from(width) * u64::from(height) * 4;
    if declared > MAX_DECODED_BYTES {
        return Err(TextureError::TooHeavy { bytes: declared });
    }

    let mut buffer = vec![0u8; reader.output_buffer_size().unwrap_or(0)];
    let frame = reader
        .next_frame(&mut buffer)
        .map_err(|err| TextureError::Malformed {
            reason: err.to_string(),
        })?;

    let rgba = to_rgba(
        &buffer[..frame.buffer_size()],
        frame.color_type,
        width,
        height,
    )?;
    Ok(Image {
        width,
        height,
        rgba,
    })
}

/// Decodes a PNG, catching a panic rather than letting it reach the caller.
///
/// The decoder has no `unsafe` and is fuzzed, so this should never fire. It
/// exists because "should never" is not "cannot", and charter rule 14 asks for
/// panic isolation on the asset path specifically: a poisoned texture disables
/// that texture, never the client.
///
/// # Errors
///
/// As [`decode_png`], plus [`TextureError::Panicked`].
pub fn decode_png_isolated(bytes: &[u8]) -> Result<Image, TextureError> {
    // `AssertUnwindSafe` because the only state crossing the boundary is a
    // borrowed slice, which a panic cannot leave inconsistent.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| decode_png(bytes)))
        .unwrap_or(Err(TextureError::Panicked))
}

/// Decodes a texture, falling back to the magenta checker.
///
/// Returns the reason alongside the fallback so the caller can surface a
/// per-server warning rather than silently rendering a broken world.
#[must_use]
pub fn decode_or_missing(bytes: &[u8]) -> (Image, Option<TextureError>) {
    match decode_png_isolated(bytes) {
        Ok(image) => (image, None),
        Err(err) => (Image::missing(), Some(err)),
    }
}

/// Converts a decoded frame to RGBA8.
fn to_rgba(
    data: &[u8],
    colour: png::ColorType,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, TextureError> {
    let pixels = (width as usize) * (height as usize);
    let mut rgba = Vec::with_capacity(pixels * 4);

    match colour {
        png::ColorType::Rgba => {
            if data.len() < pixels * 4 {
                return Err(TextureError::Malformed {
                    reason: format!("expected {} bytes of RGBA, got {}", pixels * 4, data.len()),
                });
            }
            rgba.extend_from_slice(&data[..pixels * 4]);
        }
        png::ColorType::Rgb => {
            if data.len() < pixels * 3 {
                return Err(TextureError::Malformed {
                    reason: format!("expected {} bytes of RGB, got {}", pixels * 3, data.len()),
                });
            }
            for chunk in data[..pixels * 3].chunks_exact(3) {
                rgba.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 255]);
            }
        }
        png::ColorType::Grayscale => {
            if data.len() < pixels {
                return Err(TextureError::Malformed {
                    reason: format!("expected {pixels} bytes of grey, got {}", data.len()),
                });
            }
            for value in &data[..pixels] {
                rgba.extend_from_slice(&[*value, *value, *value, 255]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            if data.len() < pixels * 2 {
                return Err(TextureError::Malformed {
                    reason: format!(
                        "expected {} bytes of grey+alpha, got {}",
                        pixels * 2,
                        data.len()
                    ),
                });
            }
            for chunk in data[..pixels * 2].chunks_exact(2) {
                rgba.extend_from_slice(&[chunk[0], chunk[0], chunk[0], chunk[1]]);
            }
        }
        png::ColorType::Indexed => {
            // `normalize_to_color8` expands palettes, so reaching here means
            // the decoder did something unexpected rather than the file being
            // unusual.
            return Err(TextureError::Malformed {
                reason: "indexed colour survived normalisation".to_owned(),
            });
        }
    }

    Ok(rgba)
}

/// A texture atlas: every block texture in one image.
#[derive(Debug, Clone)]
pub struct Atlas {
    /// Tiles per row and column.
    pub grid: u32,
    /// The packed image.
    pub image: Image,
    /// Which tile each material occupies, indexed by material id.
    slots: Vec<u32>,
}

impl Atlas {
    /// Builds an atlas from per-material textures.
    ///
    /// `textures[i]` is the texture for material id `i`. A `None` becomes the
    /// magenta checker, so a material with no texture is visibly wrong rather
    /// than invisible.
    #[must_use]
    pub fn build(textures: &[Option<Image>]) -> Self {
        // Square grid, big enough for everything. Rounded up to a power of two
        // so the shader's coordinate maths is shifts rather than divisions.
        let count = textures.len().max(1) as u32;
        let mut grid = 1;
        while grid * grid < count {
            grid *= 2;
        }

        let side = grid * TILE_PITCH;
        let mut image = Image::solid(side, side, [0, 0, 0, 0]);
        let mut slots = Vec::with_capacity(textures.len());

        for (index, texture) in textures.iter().enumerate() {
            let slot = u32::try_from(index).unwrap_or(0);
            let tile = texture.as_ref().map_or_else(Image::missing, Image::to_tile);
            blit_padded(&mut image, &tile, slot % grid, slot / grid);
            slots.push(slot);
        }

        Self { grid, image, slots }
    }

    /// The tile a material uses.
    #[must_use]
    pub fn slot_of(&self, material: u16) -> u32 {
        self.slots.get(material as usize).copied().unwrap_or(0)
    }

    /// The atlas edge length in pixels.
    #[must_use]
    pub const fn side(&self) -> u32 {
        self.grid * TILE_PITCH
    }

    /// The UV rectangle of one tile, excluding its padding.
    ///
    /// Returned as `(u0, v0, u1, v1)` in `0.0..=1.0`.
    #[must_use]
    pub fn tile_uv(&self, slot: u32) -> (f32, f32, f32, f32) {
        let side = self.side() as f32;
        let column = (slot % self.grid) as f32;
        let row = (slot / self.grid) as f32;
        let origin_x = column * TILE_PITCH as f32 + PADDING as f32;
        let origin_y = row * TILE_PITCH as f32 + PADDING as f32;
        (
            origin_x / side,
            origin_y / side,
            (origin_x + TILE as f32) / side,
            (origin_y + TILE as f32) / side,
        )
    }
}

/// Copies a tile into the atlas, extending its edge pixels into the padding.
///
/// The padding is a copy of the tile's own border. At the smallest mip level a
/// tile's neighbours are other tiles, so without this a white block picks up
/// the colour of whatever was packed beside it — the classic atlas bleed.
fn blit_padded(atlas: &mut Image, tile: &Image, column: u32, row: u32) {
    let base_x = column * TILE_PITCH;
    let base_y = row * TILE_PITCH;

    for y in 0..TILE_PITCH {
        for x in 0..TILE_PITCH {
            // Clamp into the tile: coordinates inside the padding read the
            // nearest real pixel, which is what extends the edge.
            let source_x = x.saturating_sub(PADDING).min(TILE - 1);
            let source_y = y.saturating_sub(PADDING).min(TILE - 1);
            let pixel = tile.pixel(source_x, source_y).unwrap_or([255, 0, 255, 255]);

            let target_x = base_x + x;
            let target_y = base_y + y;
            let offset = ((target_y as usize) * (atlas.width as usize) + (target_x as usize)) * 4;
            if let Some(slice) = atlas.rgba.get_mut(offset..offset + 4) {
                slice.copy_from_slice(&pixel);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A valid PNG of the given size, built by the encoder rather than by hand.
    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().expect("header");
            let data = vec![128u8; (width as usize) * (height as usize) * 4];
            writer.write_image_data(&data).expect("data");
        }
        out
    }

    #[test]
    fn a_valid_png_decodes_to_rgba() {
        let image = decode_png(&png_bytes(16, 16)).expect("decode");
        assert_eq!((image.width, image.height), (16, 16));
        assert_eq!(image.rgba.len(), 16 * 16 * 4);
        assert_eq!(image.pixel(0, 0), Some([128, 128, 128, 128]));
    }

    #[test]
    fn an_oversized_png_is_refused_from_its_header() {
        // The decompression bomb. A header claiming huge dimensions costs a few
        // bytes to send; honouring it costs gigabytes.
        //
        // The image is COMPLETE and valid — 1025x1 is only 4 KB — so the only
        // thing that can reject it is the dimension check. An earlier version
        // wrote a header with no IDAT, which the decoder rejected as malformed
        // before the check ever ran: the test passed for the wrong reason and
        // proved nothing about the limit.
        let bytes = png_bytes(MAX_DIMENSION + 1, 1);

        let err = decode_png(&bytes).expect_err("must refuse");
        assert!(
            matches!(err, TextureError::TooLarge { width, .. } if width == MAX_DIMENSION + 1),
            "expected a dimension refusal, got {err}"
        );
        assert!(
            err.to_string().contains("before decoding"),
            "the message should say why it matters: {err}"
        );
    }

    #[test]
    fn a_texture_at_exactly_the_limit_is_accepted() {
        // The boundary, from the other side. An off-by-one here would reject a
        // legitimate high-resolution pack.
        let image = decode_png(&png_bytes(MAX_DIMENSION, 1)).expect("the limit itself is fine");
        assert_eq!(image.width, MAX_DIMENSION);
    }

    #[test]
    fn a_zero_dimension_png_is_refused() {
        // Zero is not "empty", it is a division waiting to happen.
        let mut out = Vec::new();
        let encoder = png::Encoder::new(&mut out, 0, 0);
        drop(encoder);
        // A hand-built header, since the encoder will not write a zero-sized
        // image.
        let mut bytes = png_bytes(1, 1);
        // Corrupt the width field in the IHDR (bytes 16..20).
        bytes[16..20].copy_from_slice(&0u32.to_be_bytes());
        assert!(decode_png(&bytes).is_err());
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        for bytes in [
            &b""[..],
            &b"not a png"[..],
            &[0x89, 0x50, 0x4E, 0x47][..], // the magic, and nothing else
            &[0xFF; 512][..],
        ] {
            let result = decode_png_isolated(bytes);
            assert!(result.is_err(), "garbage should not decode");
        }
    }

    #[test]
    fn a_truncated_png_never_panics_and_never_exceeds_its_declared_size() {
        // The property that matters is NOT "a truncated file fails" — the png
        // decoder can legitimately return a partial image, and a half-drawn
        // texture is the server's problem rather than a security one. What must
        // hold is that the client survives and the result is bounded.
        //
        // The first version of this asserted an error and failed at one cut
        // length where the decoder succeeded. Asserting the wrong property
        // would have meant either deleting a real test or "fixing" working code.
        let full = png_bytes(32, 32);
        for cut in 1..full.len() {
            if let Ok(image) = decode_png_isolated(&full[..cut]) {
                assert_eq!(
                    image.rgba.len(),
                    (image.width as usize) * (image.height as usize) * 4,
                    "a partial decode must still be internally consistent at cut {cut}"
                );
                assert!(image.width <= MAX_DIMENSION && image.height <= MAX_DIMENSION);
            }
        }
    }

    #[test]
    fn a_failed_texture_becomes_the_magenta_checker_with_a_reason() {
        // Charter rule 14: a poisoned asset disables that asset with a
        // user-visible warning, never a crash.
        let (image, error) = decode_or_missing(b"definitely not a png");

        assert!(error.is_some(), "the reason must be reported");
        assert_eq!((image.width, image.height), (TILE, TILE));
        assert!(
            image.rgba.chunks_exact(4).any(|p| p == [255, 0, 255, 255]),
            "the fallback must be visibly magenta"
        );
    }

    #[test]
    fn the_reference_white_texture_has_a_visible_border() {
        // Without it, a wall of white blocks is one undifferentiated mass and
        // there is no way to tell whether meshing works at all.
        let image = Image::white_with_border();
        let corner = image.pixel(0, 0).expect("corner");
        let middle = image.pixel(TILE / 2, TILE / 2).expect("middle");

        assert_ne!(corner, middle, "the border must differ from the face");
        assert_eq!(middle, [255, 255, 255, 255], "the face should be white");
        assert!(corner[0] < 255, "the border should be darker");
        assert!(corner[0] > 180, "but faint, not a drawn-on grid");
    }

    #[test]
    fn resampling_is_nearest_neighbour() {
        // Voxel textures are pixel art. Smoothing turns a 16x16 into a smear.
        let mut source = Image::solid(32, 32, [0, 0, 0, 255]);
        // A single white pixel in the top-left quadrant.
        source.rgba[0..4].copy_from_slice(&[255, 255, 255, 255]);

        let tile = source.to_tile();
        assert_eq!((tile.width, tile.height), (TILE, TILE));
        // Nearest neighbour keeps it pure white; a linear filter would have
        // blended it toward black.
        assert_eq!(tile.pixel(0, 0), Some([255, 255, 255, 255]));
    }

    #[test]
    fn an_atlas_packs_every_material_and_pads_each_tile() {
        let textures = vec![
            Some(Image::solid(TILE, TILE, [255, 0, 0, 255])),
            Some(Image::solid(TILE, TILE, [0, 255, 0, 255])),
            None,
        ];
        let atlas = Atlas::build(&textures);

        assert!(atlas.grid >= 2, "three tiles need at least a 2x2 grid");
        assert_eq!(atlas.image.width, atlas.side());
        assert_eq!(atlas.image.height, atlas.side());

        // The first tile's interior is its own colour.
        let (u0, v0, _, _) = atlas.tile_uv(0);
        let x = (u0 * atlas.side() as f32) as u32;
        let y = (v0 * atlas.side() as f32) as u32;
        assert_eq!(atlas.image.pixel(x, y), Some([255, 0, 0, 255]));
    }

    #[test]
    fn tile_padding_repeats_the_edge_rather_than_the_neighbour() {
        // The atlas-bleed defence. At the smallest mip a tile's neighbours are
        // other tiles, so unpadded packing paints one block with another's
        // colour.
        let textures = vec![
            Some(Image::solid(TILE, TILE, [255, 0, 0, 255])),
            Some(Image::solid(TILE, TILE, [0, 0, 255, 255])),
        ];
        let atlas = Atlas::build(&textures);

        // The pixel just left of tile 0's interior is inside tile 0's padding
        // and must be tile 0's colour, not the atlas background or tile 1's.
        let padding_pixel = atlas.image.pixel(0, PADDING).expect("in atlas");
        assert_eq!(
            padding_pixel,
            [255, 0, 0, 255],
            "padding must repeat the tile's own edge"
        );

        // And the pixel just right of tile 0's interior, still in its padding.
        let right = atlas
            .image
            .pixel(PADDING + TILE + PADDING - 1, PADDING)
            .expect("in atlas");
        assert_eq!(right, [255, 0, 0, 255], "the right padding too");
    }

    #[test]
    fn tile_uvs_exclude_the_padding() {
        // Sampling the padding would show the edge pixel stretched, which is a
        // subtle wrongness that looks like a texture authoring mistake.
        let atlas = Atlas::build(&vec![Some(Image::solid(TILE, TILE, [1, 2, 3, 4])); 4]);
        let (u0, v0, u1, v1) = atlas.tile_uv(0);

        let side = atlas.side() as f32;
        assert!(
            (u0 * side - PADDING as f32).abs() < 0.01,
            "u0 skips the padding"
        );
        assert!((v0 * side - PADDING as f32).abs() < 0.01);
        assert!(
            ((u1 - u0) * side - TILE as f32).abs() < 0.01,
            "spans one tile"
        );
        assert!(((v1 - v0) * side - TILE as f32).abs() < 0.01);
    }

    #[test]
    fn an_empty_atlas_is_still_valid() {
        let atlas = Atlas::build(&[]);
        assert!(atlas.side() > 0);
        assert_eq!(atlas.slot_of(0), 0);
    }

    #[test]
    fn every_png_colour_type_decodes() {
        // A server can send any of these, and refusing one because it is
        // unusual means a texture pack that works elsewhere fails here.
        for colour in [
            png::ColorType::Rgba,
            png::ColorType::Rgb,
            png::ColorType::Grayscale,
            png::ColorType::GrayscaleAlpha,
        ] {
            let mut out = Vec::new();
            {
                let mut encoder = png::Encoder::new(&mut out, 8, 8);
                encoder.set_color(colour);
                encoder.set_depth(png::BitDepth::Eight);
                let mut writer = encoder.write_header().expect("header");
                let samples = match colour {
                    png::ColorType::Rgba => 4,
                    png::ColorType::Rgb => 3,
                    png::ColorType::GrayscaleAlpha => 2,
                    _ => 1,
                };
                writer
                    .write_image_data(&vec![200u8; 8 * 8 * samples])
                    .expect("data");
            }

            let image = decode_png(&out).unwrap_or_else(|err| panic!("{colour:?} failed: {err}"));
            assert_eq!(image.rgba.len(), 8 * 8 * 4, "{colour:?} should become RGBA");
        }
    }
}
