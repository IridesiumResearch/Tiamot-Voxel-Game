// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The screenshot smoke test: render a fixed scene and check it did not
//! silently break.
//!
//! # What this gate is and is not
//!
//! It is a check that the renderer still produces a picture of the world. It is
//! **not** a pixel-exact comparison, because two drivers never agree to the
//! bit — rasterisation rules, filtering, and floating point all move colours
//! slightly. The frame is reduced to a coarse grid of averaged cells before it
//! is hashed, and beside that hash the tests assert *structural* properties
//! that hold on any conforming driver: the sky is above the ground, a lit face
//! is brighter than a shadowed one, an edit changes the picture.
//!
//! The structural assertions are the ones that catch a real regression. The
//! hash catches the changes nobody thought to assert about.
//!
//! # Skipping, and when skipping is not allowed
//!
//! A machine with no Vulkan, Metal, DX12, or GL adapter cannot run these, and
//! failing there would mean `cargo test` did not pass on a headless developer
//! box — which teaches people to ignore failures. So they skip, loudly.
//!
//! **CI sets `TIAMOT_REQUIRE_GPU=1`**, which turns a missing adapter into a
//! failure. Without that, a broken CI image would quietly stop testing
//! rendering and nothing would say so.

use client::camera::{Camera, Position};
use client::config::RenderMode;
use client::mesher::{self, Absent, Neighbours};
use client::render::offscreen::{hash_hex, perceptual_hash};
use client::render::{Gpu, Offscreen, Renderer};
use client::texture::{Atlas, Image};
use tiamot_core::{BlockPos, BlockValue, Chunk, ChunkPos, MaterialId};

/// Frame size for every test here.
///
/// Small on purpose: a software rasteriser is the target, and 320x240 renders
/// in milliseconds while still being far more than the 16x16 hash grid needs.
const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;

/// The material the fixed scene is built from.
const STONE: MaterialId = MaterialId(2);

/// Opens a device, or explains why the test is being skipped.
///
/// Returns `None` when there is no adapter and CI has not demanded one.
fn gpu() -> Option<Gpu> {
    match Gpu::headless() {
        Ok(gpu) => {
            println!("rendering on `{}` via {}", gpu.adapter, gpu.backend);
            Some(gpu)
        }
        Err(err) => {
            assert!(
                std::env::var("TIAMOT_REQUIRE_GPU").is_err(),
                "TIAMOT_REQUIRE_GPU is set and no adapter was available: {err}"
            );
            println!(
                "SKIPPING: no graphics adapter on this machine ({err}). Set TIAMOT_REQUIRE_GPU=1 \
                 to make this a failure."
            );
            None
        }
    }
}

/// The fixed scene: a solid floor with one block dug out of it, and one block
/// chiselled to sub-node resolution standing on top.
///
/// Fixed in the strong sense — no randomness, no clock, no world seed. A
/// screenshot gate whose input varied would be a gate that failed for reasons
/// nobody could reproduce.
fn scene_at(origin: ChunkPos) -> Vec<Chunk> {
    // Three chunks square, not one. A single chunk is sixteen blocks across and
    // cannot fill a 70-degree frame from a height where anything is legible, so
    // the frame ends up mostly sky and the structural assertions have nothing
    // to read. Nine chunks also means the frame exercises what one cannot:
    // several draw calls, several instance-buffer entries, and border culling
    // between real neighbours.
    let mut chunks = Vec::with_capacity(9);

    for cx in 0..3 {
        for cz in 0..3 {
            let pos = ChunkPos::new(origin.x + cx, origin.y, origin.z + cz);
            // Built relative to each chunk's own corner, so the identical scene
            // can be placed anywhere in the world — which is what the
            // floating-origin frame comparison needs.
            let corner = BlockPos::from_chunk_corner(pos);
            let at = |dx: i32, dy: i32, dz: i32| {
                BlockPos::new(corner.x + dx, corner.y + dy, corner.z + dz)
            };

            let mut chunk = Chunk::new(pos, MaterialId::AIR);
            for x in 0..16 {
                for z in 0..16 {
                    for y in 0..8 {
                        chunk
                            .set_block(at(x, y, z), BlockValue::Uniform(STONE))
                            .expect("in chunk");
                    }
                }
            }

            if (cx, cz) == (1, 1) {
                // A hole, so the frame has shadowed faces in it and not only
                // lit tops.
                chunk
                    .set_block(at(8, 7, 8), BlockValue::Uniform(MaterialId::AIR))
                    .expect("in chunk");
                // A block standing proud with one sub-node cell removed: the
                // engine's headline feature, in the picture the gate hashes.
                chunk
                    .set_block(at(4, 8, 4), BlockValue::Uniform(STONE))
                    .expect("in chunk");
                chunk
                    .set_subnode(at(4, 8, 4).subnode(2, 2, 2), MaterialId::AIR)
                    .expect("in chunk");
            }

            chunks.push(chunk);
        }
    }

    chunks
}

/// The fixed scene at the world origin.
fn scene() -> Vec<Chunk> {
    scene_at(ChunkPos::new(0, 0, 0))
}

/// A camera looking down at the scene from above it.
///
/// Placed and angled so the floor fills the bottom of the frame and open sky
/// fills the top — which is what the structural assertions read.
///
/// Getting this wrong is easy and the failure is confusing: an earlier version
/// sat outside the floor's footprint looking in at a shallow angle, so the
/// BOTTOM of the frame looked past the floor's near edge into empty space and
/// came back as sky. The scene was fine and the camera was not.
fn viewpoint() -> Camera {
    let mut camera = Camera {
        position: Position::from_world(24.0, 18.0, 20.0),
        ..Camera::default()
    };
    camera.look(0.0, -0.9);
    camera
}

/// Uploads a scene and returns a renderer ready to draw it.
fn prepare(gpu: Gpu, chunks: &[Chunk], mode: RenderMode) -> Renderer {
    let mut renderer = Renderer::new(gpu, mode, WIDTH, HEIGHT).expect("renderer");

    // A real atlas: air, unknown, and stone. Slot indices are material ids, so
    // stone's texture must sit at index 2.
    let atlas = Atlas::build(&[None, None, Some(Image::white_with_border())]);
    renderer.set_atlas(&atlas);

    upload(&mut renderer, chunks);
    renderer
}

/// Meshes every chunk against its real neighbours and uploads it.
///
/// The neighbours matter: meshed in isolation, the nine chunks would each draw
/// a wall of faces at every shared plane, and the frame would be full of
/// z-fighting quads that no assertion here would notice.
fn upload(renderer: &mut Renderer, chunks: &[Chunk]) {
    let by_pos: std::collections::BTreeMap<ChunkPos, &Chunk> =
        chunks.iter().map(|chunk| (chunk.pos(), chunk)).collect();

    for chunk in chunks {
        let pos = chunk.pos();
        let mut neighbours = Neighbours::none();
        for (index, (dx, dy, dz)) in [
            (-1, 0, 0),
            (1, 0, 0),
            (0, -1, 0),
            (0, 1, 0),
            (0, 0, -1),
            (0, 0, 1),
        ]
        .into_iter()
        .enumerate()
        {
            neighbours.sides[index] = by_pos
                .get(&ChunkPos::new(pos.x + dx, pos.y + dy, pos.z + dz))
                .copied();
        }
        let mesh = mesher::mesh_chunk(chunk, &neighbours, Absent::Air);
        renderer.set_chunk(pos, &mesh);
    }
}

/// The average colour of a rectangle, as fractions of full brightness.
fn average(image: &Image, x0: u32, y0: u32, x1: u32, y1: u32) -> [f32; 3] {
    let mut total = [0u64; 3];
    let mut samples = 0u64;
    for y in y0..y1 {
        for x in x0..x1 {
            if let Some(pixel) = image.pixel(x, y) {
                for channel in 0..3 {
                    total[channel] += u64::from(pixel[channel]);
                }
                samples += 1;
            }
        }
    }
    let samples = samples.max(1) as f32;
    [
        total[0] as f32 / samples / 255.0,
        total[1] as f32 / samples / 255.0,
        total[2] as f32 / samples / 255.0,
    ]
}

/// Whether a colour is more blue than it is red — i.e. sky rather than stone.
fn is_sky(colour: [f32; 3]) -> bool {
    colour[2] > colour[0] + 0.05
}

#[test]
fn a_frame_of_the_fixed_scene_has_sky_above_and_world_below() {
    // The structural assertion that catches almost every real regression: a
    // world that stopped drawing is all sky, a camera pointing the wrong way
    // has them swapped, and an inverted depth test puts the floor above the
    // horizon.
    let Some(gpu) = gpu() else { return };
    let chunks = scene();
    let mut renderer = prepare(gpu, &chunks, RenderMode::Textured);
    let target = Offscreen::new(renderer.gpu(), WIDTH, HEIGHT);

    let frame = target
        .capture(&mut renderer, &viewpoint())
        .expect("capture");
    println!("frame hash: {}", hash_hex(&frame));

    let top = average(&frame, 0, 0, WIDTH, HEIGHT / 8);
    let bottom = average(&frame, 0, HEIGHT * 3 / 4, WIDTH, HEIGHT);

    assert!(
        is_sky(top),
        "the top of the frame should be sky, got {top:?}"
    );
    assert!(
        !is_sky(bottom),
        "the bottom of the frame should be the world, got {bottom:?}"
    );
    assert!(
        renderer.drawn() > 0,
        "the frustum culled everything; nothing was drawn"
    );
}

#[test]
fn directional_shading_makes_the_geometry_legible() {
    // Lighting mode 1, as a picture rather than as a unit test of the constant.
    // Flat-lit voxels are one white mass; if this ever fails, the shader's
    // face_shade has drifted from the mesher's.
    let Some(gpu) = gpu() else { return };
    let chunks = scene();
    let mut renderer = prepare(gpu, &chunks, RenderMode::Textured);
    let target = Offscreen::new(renderer.gpu(), WIDTH, HEIGHT);
    let frame = target
        .capture(&mut renderer, &viewpoint())
        .expect("capture");

    // The camera looks down and north, so the frame holds both up-facing tops
    // (shade 1.0) and south-facing sides (shade 0.85). Sampled as a spread
    // rather than at two chosen pixels, because which pixel is which face
    // depends on rasterisation.
    let mut brightest = 0.0f32;
    let mut darkest = 1.0f32;
    for y in (HEIGHT / 2..HEIGHT).step_by(4) {
        for x in (0..WIDTH).step_by(4) {
            let colour = average(&frame, x, y, x + 4, y + 4);
            if is_sky(colour) {
                continue;
            }
            let luminance = (colour[0] + colour[1] + colour[2]) / 3.0;
            brightest = brightest.max(luminance);
            darkest = darkest.min(luminance);
        }
    }

    assert!(
        brightest - darkest > 0.1,
        "every visible surface has the same brightness ({darkest} to {brightest}); directional \
         shading is not reaching the fragment shader"
    );
}

#[test]
fn the_same_scene_renders_to_the_same_hash_twice() {
    // Determinism of the gate itself. A hash that varied run to run would make
    // every future comparison meaningless, and the cause would be something
    // real — an uninitialised buffer, a race in the instance array.
    let Some(gpu) = gpu() else { return };
    let chunks = scene();
    let mut renderer = prepare(gpu, &chunks, RenderMode::Textured);
    let target = Offscreen::new(renderer.gpu(), WIDTH, HEIGHT);

    let first = target
        .capture(&mut renderer, &viewpoint())
        .expect("capture");
    let second = target
        .capture(&mut renderer, &viewpoint())
        .expect("capture");

    assert_eq!(
        perceptual_hash(&first),
        perceptual_hash(&second),
        "the same scene rendered twice must produce the same frame"
    );
}

#[test]
fn an_edit_changes_the_picture() {
    // The remesh path, proven where it matters: a block removed must show up in
    // the pixels. This is the [A]-assertable half of "block edits made by a bot
    // appear live in the client".
    let Some(gpu) = gpu() else { return };
    let chunks = scene();
    let mut renderer = prepare(gpu, &chunks, RenderMode::Textured);
    let target = Offscreen::new(renderer.gpu(), WIDTH, HEIGHT);
    let before = target
        .capture(&mut renderer, &viewpoint())
        .expect("capture");

    // Dig a trench across the floor — the same path a BlockDelta takes: apply
    // to the chunk, remesh, re-upload.
    let mut edited: Vec<Chunk> = chunks.clone();
    let corner = BlockPos::from_chunk_corner(edited[4].pos());
    for x in 0..16 {
        edited[4]
            .set_block(
                BlockPos::new(corner.x + x, corner.y + 7, corner.z + 6),
                BlockValue::Uniform(MaterialId::AIR),
            )
            .expect("in chunk");
    }
    upload(&mut renderer, &edited);

    let after = target
        .capture(&mut renderer, &viewpoint())
        .expect("capture");
    assert_ne!(
        perceptual_hash(&before),
        perceptual_hash(&after),
        "digging a trench across the floor did not change the frame; the remesh never reached \
         the GPU"
    );
}

#[test]
fn digging_out_the_world_leaves_only_sky() {
    // The other direction, and the one that would catch a renderer drawing a
    // stale mesh forever: with nothing left to draw, the frame must be the
    // clear colour and nothing else.
    let Some(gpu) = gpu() else { return };
    let chunks = scene();
    let mut renderer = prepare(gpu, &chunks, RenderMode::Textured);
    let target = Offscreen::new(renderer.gpu(), WIDTH, HEIGHT);

    let emptied: Vec<Chunk> = chunks
        .iter()
        .map(|chunk| Chunk::new(chunk.pos(), MaterialId::AIR))
        .collect();
    upload(&mut renderer, &emptied);
    assert_eq!(
        renderer.chunk_count(),
        0,
        "an empty mesh must remove the chunk rather than keep a zero-length draw"
    );

    let frame = target
        .capture(&mut renderer, &viewpoint())
        .expect("capture");
    for (label, y) in [("top", 0), ("middle", HEIGHT / 2), ("bottom", HEIGHT - 8)] {
        let colour = average(&frame, 0, y, WIDTH, y + 8);
        assert!(
            is_sky(colour),
            "the {label} of an emptied world should be sky, got {colour:?}"
        );
    }
}

#[test]
fn flat_mode_draws_the_same_geometry_without_the_atlas() {
    // The diagnostic the render mode exists for: if the world looks right in
    // flat and wrong in textured, the mesher is fine and the atlas is not. Both
    // modes must agree about where the geometry IS.
    let Some(gpu) = gpu() else { return };
    let chunks = scene();
    let mut flat = prepare(gpu, &chunks, RenderMode::Flat);
    let target = Offscreen::new(flat.gpu(), WIDTH, HEIGHT);
    let frame = target.capture(&mut flat, &viewpoint()).expect("capture");

    let top = average(&frame, 0, 0, WIDTH, HEIGHT / 8);
    let bottom = average(&frame, 0, HEIGHT * 3 / 4, WIDTH, HEIGHT);
    assert!(is_sky(top), "flat mode still has a sky: {top:?}");
    assert!(!is_sky(bottom), "and still has a world: {bottom:?}");
}

#[test]
fn the_frame_is_identical_at_the_origin_and_at_the_edge_of_the_world() {
    // Floating origin, as pixels. The unit tests prove the draw offsets match;
    // this proves nothing downstream of them reintroduces a world coordinate.
    //
    // Fifty thousand blocks out, an f32 world position has a representable step
    // coarser than a hundredth of a sub-node — so a renderer that had one
    // anywhere would produce a visibly different frame here.
    let Some(gpu) = gpu() else { return };

    let near = scene();
    let mut renderer = prepare(gpu, &near, RenderMode::Textured);
    let target = Offscreen::new(renderer.gpu(), WIDTH, HEIGHT);
    let here = target
        .capture(&mut renderer, &viewpoint())
        .expect("capture");

    // The same scene, built at the edge of the world, viewed from the same
    // place relative to it. 50,000 blocks is 3,125 chunks.
    let far_chunk = ChunkPos::new(3125, 0, 3125);
    let far_scene = scene_at(far_chunk);

    renderer.clear();
    upload(&mut renderer, &far_scene);

    // Derived from `viewpoint()` by translation rather than written out again.
    // Restating the coordinates is how this test silently stopped comparing
    // like with like: the near camera moved and the far one did not, so the
    // frames legitimately differed and the failure read as a floating-origin
    // bug in the renderer.
    let corner = BlockPos::from_chunk_corner(far_chunk);
    let (near_x, near_y, near_z) = viewpoint().position.to_world();
    let mut far_camera = viewpoint();
    far_camera.position = Position::from_world(
        near_x + f64::from(corner.x),
        near_y + f64::from(corner.y),
        near_z + f64::from(corner.z),
    );
    let there = target.capture(&mut renderer, &far_camera).expect("capture");

    assert_eq!(
        perceptual_hash(&here),
        perceptual_hash(&there),
        "the same scene looks different at the edge of the world; something in the render path \
         is accumulating a world-space f32"
    );
}

/// Writes the fixed scene to a PNG beside the test binary, for eyeballing.
///
/// Ignored by default — it is a debugging aid, not a gate. Run with
/// `cargo test -p client --test screenshot -- --ignored --nocapture`.
#[test]
#[ignore = "debugging aid: writes a PNG rather than asserting anything"]
fn dump_the_fixed_scene() {
    let Some(gpu) = gpu() else { return };
    let chunks = scene();
    let mut renderer = prepare(gpu, &chunks, RenderMode::Textured);
    let target = Offscreen::new(renderer.gpu(), WIDTH, HEIGHT);
    let frame = target
        .capture(&mut renderer, &viewpoint())
        .expect("capture");

    let path = std::env::temp_dir().join("tiamot-scene.png");
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, frame.width, frame.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("header");
        writer.write_image_data(&frame.rgba).expect("data");
    }
    std::fs::write(&path, &bytes).expect("write");
    println!("wrote {}", path.display());
    println!(
        "drew {} of {} chunks",
        renderer.drawn(),
        renderer.chunk_count()
    );
    for band in 0..8 {
        let y = band * HEIGHT / 8;
        println!(
            "band {band}: {:?}",
            average(&frame, 0, y, WIDTH, y + HEIGHT / 8)
        );
    }
}
