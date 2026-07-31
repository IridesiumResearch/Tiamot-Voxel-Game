// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only
//
//! Seeds the texture_ingest fuzz corpus with valid PNGs of every shape the
//! decoder accepts. A fuzzer starting from random bytes spends its budget
//! discovering that almost nothing is a PNG; seeded, it starts inside the
//! decodable space and mutates outward.
fn main() {
    let out = std::path::PathBuf::from(std::env::args().nth(1).expect("dir"));
    std::fs::create_dir_all(&out).expect("dir");
    let mut written = 0;
    for (w, h, colour, depth) in [
        (16u32, 16u32, png::ColorType::Rgba, png::BitDepth::Eight),
        (16, 16, png::ColorType::Rgb, png::BitDepth::Eight),
        (16, 16, png::ColorType::Grayscale, png::BitDepth::Eight),
        (16, 16, png::ColorType::GrayscaleAlpha, png::BitDepth::Eight),
        (1, 1, png::ColorType::Rgba, png::BitDepth::Eight),
        (32, 8, png::ColorType::Rgba, png::BitDepth::Eight),
        (1024, 1, png::ColorType::Rgba, png::BitDepth::Eight),
        (16, 16, png::ColorType::Rgba, png::BitDepth::Sixteen),
    ] {
        let mut bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut bytes, w, h);
            enc.set_color(colour);
            enc.set_depth(depth);
            let mut wr = enc.write_header().expect("header");
            let samples = match colour {
                png::ColorType::Rgba => 4,
                png::ColorType::Rgb => 3,
                png::ColorType::GrayscaleAlpha => 2,
                _ => 1,
            };
            let bytes_per = if depth == png::BitDepth::Sixteen {
                2
            } else {
                1
            };
            let data = vec![170u8; (w as usize) * (h as usize) * samples * bytes_per];
            wr.write_image_data(&data).expect("data");
        }
        let name = format!("{w}x{h}-{colour:?}-{depth:?}")
            .to_lowercase()
            .replace("::", "-");
        std::fs::write(out.join(&name), &bytes).expect("write");
        written += 1;
    }
    println!("wrote {written} texture seeds");
}
