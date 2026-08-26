// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only
//
//! Regenerates the PNGs shipped by the reference mods in `game/`.
//!
//! The images are *defined* in Rust — [`client::texture::Image`] — and written
//! out here, rather than being hand-drawn and checked in as opaque bytes. A
//! checked-in PNG nobody can regenerate is a file that drifts: someone opens it
//! in an editor, saves it with a different gamma, and the "faint border" that
//! made block edges readable is gone with no diff anyone can read.
//!
//! `a_shipped_reference_texture_matches_the_image_it_was_generated_from` pins
//! the two together, so this only ever needs running when the definition
//! changes.
//!
//! Usage: `cargo run -p client --example write_reference_textures -- game`

fn main() {
    let root =
        std::path::PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| "game".to_owned()));

    write(
        &root.join("core_blocks/textures/white.png"),
        &client::texture::Image::white_with_border(),
    );

    // The saturation chain `core_milk` demonstrates: the same ground, darker
    // each time it drinks. Sub-Node Contract §4.3 makes saturation a chain of
    // MATERIALS rather than state bits, and this is the half of that decision
    // the mod owns — the engine has no opinion about what wet dirt looks like.
    for (name, colour) in client::texture::GROUND_CHAIN {
        write(
            &root.join(format!("core_milk/textures/{name}.png")),
            &client::texture::Image::tinted_with_border(*colour),
        );
    }
}

/// Encodes an image as RGBA8 PNG and writes it, creating parent directories.
fn write(path: &std::path::Path, image: &client::texture::Image) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create texture directory");
    }
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, image.width, image.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        writer.write_image_data(&image.rgba).expect("png data");
    }
    std::fs::write(path, &bytes).expect("write texture");
    println!("wrote {} ({} bytes)", path.display(), bytes.len());
}
