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

use client::app::TELEPORT_CHUNKS;
use client::camera::{Camera, Position};
use client::config::{LightingMode, RenderMode, ShadowQuality};
use client::mesher::{self, Absent, Neighbours};
use client::render::offscreen::{hash_hex, perceptual_hash};
use client::render::{Gpu, Offscreen, Renderer};
use client::texture::{Atlas, Image};
use tiamot_core::proto::SkyGrade;
use tiamot_core::{BlockPos, BlockValue, Chunk, ChunkPos, MaterialId};

/// Full daylight, so these scenes measure what they are about rather than the
/// light that has not arrived for them.
const DAY: client::shade::Uniform = client::shade::Uniform(tiamot_core::light::Light::DAYLIGHT);

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
    upload_lit(renderer, chunks, &DAY);
}

/// The same, with a chosen stored light rather than full daylight.
///
/// For the tests that are about what the shader does with a light level, which
/// need one the server would really have produced — a sealed room's zero, above
/// all, which no scene built out of daylight can offer.
fn upload_lit(renderer: &mut Renderer, chunks: &[Chunk], light: &impl client::shade::BlockLight) {
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
        let mesh = mesher::mesh_chunk(chunk, &neighbours, Absent::Air, light);
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
fn distant_terrain_fades_into_the_sky() {
    // **Fog exists to hide the edge of the loaded world.** Without it, the far
    // chunk boundary is a hard line between terrain and sky and every chunk
    // arrives at full contrast; with it, distance dissolves into the same
    // colour the frame is cleared to.
    //
    // Asserted as a comparison rather than an absolute: the near floor and the
    // far floor are the same material under the same light, so any difference
    // between them is the fog.
    let Some(gpu) = gpu() else { return };
    let chunks = scene();
    let mut renderer = prepare(gpu, &chunks, RenderMode::Textured);
    let target = Offscreen::new(renderer.gpu(), WIDTH, HEIGHT);

    // Fog well inside the scene, so there is near ground and far ground in one
    // frame. The sky colour is the clear colour, which is what makes the two
    // meet without a seam.
    // Aggressive on purpose: the fixture's floor is white and the sky is a
    // pale blue, so a gentle fog moves the colour by a few thousandths and the
    // assertion would be measuring rounding. Eight blocks puts the far ground
    // most of the way to sky.
    renderer.set_sky(client::render::sky_colour(), 16.0);
    let hazy = target
        .capture(&mut renderer, &viewpoint())
        .expect("capture");

    // The same frame with fog pushed past everything, as the control.
    renderer.set_sky(client::render::sky_colour(), 100_000.0);
    let clear = target
        .capture(&mut renderer, &viewpoint())
        .expect("capture");

    // Just below the horizon is the most distant ground in the frame.
    let far_hazy = average(&hazy, 0, HEIGHT * 9 / 20, WIDTH, HEIGHT / 2);
    let far_clear = average(&clear, 0, HEIGHT * 9 / 20, WIDTH, HEIGHT / 2);
    let near_hazy = average(&hazy, 0, HEIGHT * 7 / 8, WIDTH, HEIGHT);
    let near_clear = average(&clear, 0, HEIGHT * 7 / 8, WIDTH, HEIGHT);

    // The control is a white floor under white light, so its blueness is
    // exactly zero: any separation from it is the fog.
    let blueness = |colour: [f32; 3]| colour[2] - colour[0];
    assert!(
        blueness(far_hazy) > blueness(far_clear) + 0.004,
        "distant ground did not move towards the sky: {far_hazy:?} against {far_clear:?}"
    );

    // **The property that makes this fog rather than a tint**: it deepens with
    // distance. Stated within one frame rather than against an absolute,
    // because how far away the bottom of the frame is depends on where the
    // fixture's camera stands — and a test encoding that breaks when the
    // viewpoint moves for unrelated reasons.
    assert!(
        blueness(far_hazy) > blueness(near_hazy) + 0.004,
        "fog did not deepen with distance: near {near_hazy:?}, far {far_hazy:?}"
    );
    // The unfogged frame has no such gradient, which is what makes the
    // comparison above about the fog rather than about the scene's shading.
    assert!(
        (blueness(far_clear) - blueness(near_clear)).abs() < 0.004,
        "the control frame already had a distance gradient: near {near_clear:?}, far \
         {far_clear:?}"
    );
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
fn the_selection_outline_reaches_the_pixels() {
    // The outline is a separate pipeline with its own shader, its own primitive
    // topology, and a depth state that writes nothing. Every one of those is a
    // way for it to compile, bind, issue a draw call, and put no pixels on the
    // screen — with nothing anywhere reporting a problem. The app-level test
    // asserts which CELLS are selected; this asserts that selecting them is
    // visible.
    let Some(gpu) = gpu() else { return };
    let chunks = scene();
    let mut renderer = prepare(gpu, &chunks, RenderMode::Textured);
    let target = Offscreen::new(renderer.gpu(), WIDTH, HEIGHT);

    let before = target
        .capture(&mut renderer, &viewpoint())
        .expect("capture");

    // A box right in front of the camera, in the camera-relative cells the
    // renderer expects.
    renderer.set_selection(&[[0.0, -2.0, 6.0]]);
    let after = target
        .capture(&mut renderer, &viewpoint())
        .expect("capture");

    assert_ne!(
        perceptual_hash(&before),
        perceptual_hash(&after),
        "the selection outline changed no pixels; it is being drawn into nothing"
    );

    // And it goes away again, so it is genuinely per-frame state rather than
    // something baked in on the first draw.
    renderer.set_selection(&[]);
    let cleared = target
        .capture(&mut renderer, &viewpoint())
        .expect("capture");
    assert_eq!(
        perceptual_hash(&before),
        perceptual_hash(&cleared),
        "clearing the selection left the outline on screen"
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

#[test]
fn the_debug_teleport_leaves_the_world_on_screen() {
    // Human gate 3 is "no visible jitter at ±50,000 blocks", and the first
    // version of the teleport could not show it: it moved the camera and left
    // the world at the origin, 50,000 blocks behind a 1,000-block far plane.
    // The gate ran, saw an empty sky, and passed vacuously.
    //
    // So this asserts in two halves and needs both. The frame must still be
    // the same picture — that is floating origin working — AND it must still
    // be a picture, with world in the bottom of it. Hash equality alone is
    // satisfied by two identical empty skies, which is the bug itself.
    let Some(gpu) = gpu() else { return };
    let chunks = scene();
    let mut renderer = prepare(gpu, &chunks, RenderMode::Textured);
    let target = Offscreen::new(renderer.gpu(), WIDTH, HEIGHT);

    let here = target
        .capture(&mut renderer, &viewpoint())
        .expect("capture");

    // What F8 does: displace every mesh and the camera by the same whole
    // number of chunks, so nothing moves relative to anything else.
    renderer.rebase([TELEPORT_CHUNKS, 0, TELEPORT_CHUNKS]);
    let mut far = viewpoint();
    far.position.chunk = ChunkPos::new(
        far.position.chunk.x + TELEPORT_CHUNKS,
        far.position.chunk.y,
        far.position.chunk.z + TELEPORT_CHUNKS,
    );
    let there = target.capture(&mut renderer, &far).expect("capture");

    assert!(
        renderer.drawn() > 0,
        "nothing was drawn at the edge of the world; the teleport lost the world it was \
         supposed to carry"
    );
    let bottom = average(&there, 0, HEIGHT * 3 / 4, WIDTH, HEIGHT);
    assert!(
        !is_sky(bottom),
        "the bottom of the frame is sky at 50,000 blocks out ({bottom:?}), so this gate would \
         pass on an empty screen"
    );
    assert_eq!(
        perceptual_hash(&here),
        perceptual_hash(&there),
        "the picture changed 50,000 blocks from the origin: {} here, {} there",
        hash_hex(&here),
        hash_hex(&there)
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

/// One camera placement in the six-sided face check.
struct View {
    label: &'static str,
    /// Offset from the block's centre, in blocks.
    offset: [f64; 3],
    yaw: f32,
    pitch: f32,
}

impl View {
    const fn new(label: &'static str, offset: [f64; 3], yaw: f32, pitch: f32) -> Self {
        Self {
            label,
            offset,
            yaw,
            pitch,
        }
    }
}

#[test]
fn every_face_of_a_block_is_drawn_at_its_own_brightness() {
    // THE test that was missing, and the bug it found: top and bottom faces
    // were wound the wrong way and back-face culled.
    //
    // Counting white pixels is NOT enough, and a first version of this test
    // passed while the bug was live. Looking down at a block whose top is
    // culled still shows white — the BOTTOM face, drawn through where the top
    // should have been. The picture is plausible; it is simply one layer too
    // deep and at the wrong brightness.
    //
    // So this asserts the brightness ORDERING that lighting mode 1 defines:
    // top 1.0 > z-sides 0.85 > x-sides 0.75 > bottom 0.5. A culled near face
    // means the far face's shade is measured instead, and the ordering
    // collapses — which is the only observation that separates "drawn" from
    // "drawn through".
    let Some(gpu) = gpu() else { return };

    let mut chunk = Chunk::new(ChunkPos::new(0, 0, 0), MaterialId::AIR);
    chunk
        .set_block(BlockPos::new(8, 8, 8), BlockValue::Uniform(STONE))
        .expect("in chunk");
    let chunks = vec![chunk];

    let mut renderer = prepare(gpu, &chunks, RenderMode::Textured);
    let target = Offscreen::new(renderer.gpu(), WIDTH, HEIGHT);

    // The block spans one unit at (8, 8, 8), so its centre is (8.5, 8.5, 8.5).
    const CENTRE: f64 = 8.5;
    const AWAY: f64 = 6.0;
    // Each view stands off one side of the block and looks back at it, so the
    // face it measures is the one facing the camera — a camera out at −x sees
    // the −x face. Yaw turns right from north (+z), so `forward` is
    // `(−sin yaw, ·, cos yaw)`: looking toward +x is −π/2, not +π/2. Getting
    // that backwards points every side view at empty sky, which is how this
    // test caught the inverted mouse-look.
    let views = [
        View::new("-z face", [0.0, 0.0, -AWAY], 0.0, 0.0),
        View::new("+z face", [0.0, 0.0, AWAY], std::f32::consts::PI, 0.0),
        View::new(
            "+x face",
            [AWAY, 0.0, 0.0],
            std::f32::consts::FRAC_PI_2,
            0.0,
        ),
        View::new(
            "-x face",
            [-AWAY, 0.0, 0.0],
            -std::f32::consts::FRAC_PI_2,
            0.0,
        ),
        View::new("top", [0.0, AWAY, 0.0], 0.0, -1.5),
        View::new("bottom", [0.0, -AWAY, 0.0], 0.0, 1.5),
    ];

    let mut brightness = std::collections::BTreeMap::new();
    for View {
        label,
        offset,
        yaw,
        pitch,
    } in views
    {
        let camera = Camera {
            position: Position::from_world(
                CENTRE + offset[0],
                CENTRE + offset[1],
                CENTRE + offset[2],
            ),
            yaw,
            pitch,
            ..Camera::default()
        };
        let frame = target.capture(&mut renderer, &camera).expect("capture");

        // The block is white, the sky is blue: any pixel where red has caught
        // up with blue belongs to the block.
        let mut total = 0.0;
        let mut samples = 0.0;
        for y in (0..HEIGHT).step_by(2) {
            for x in (0..WIDTH).step_by(2) {
                let Some(pixel) = frame.pixel(x, y) else {
                    continue;
                };
                if i32::from(pixel[2]) - i32::from(pixel[0]) < 20 {
                    total += f32::from(pixel[0]) / 255.0;
                    samples += 1.0;
                }
            }
        }

        assert!(
            samples > 20.0,
            "{label}: the block is invisible ({samples} pixels). That face is wound the wrong \
             way and is being back-face culled."
        );
        brightness.insert(label, total / samples);
    }

    let at = |name: &str| brightness[name];
    assert!(
        at("top") > at("+z face"),
        "the top face should be the brightest: {brightness:?}"
    );
    assert!(
        at("+z face") > at("+x face"),
        "z sides should be brighter than x sides: {brightness:?}"
    );
    assert!(
        at("+x face") > at("bottom"),
        "sides should be brighter than the bottom: {brightness:?}"
    );
    // And the opposing faces of each pair agree with each other, which they
    // cannot if one of them is really the other seen through a culled face.
    for (a, b) in [("+z face", "-z face"), ("+x face", "-x face")] {
        assert!(
            (at(a) - at(b)).abs() < 0.03,
            "{a} and {b} should be equally lit: {brightness:?}"
        );
    }
}

#[test]
fn mode_three_draws_the_world_through_its_post_chain() {
    // The chain end to end, as pixels: the world is drawn into a float target,
    // thresholded, blurred twice, and composited back through a tonemap. A
    // mistake anywhere in it — a pass reading its own output, a target bound
    // the wrong way round, a shader that fails to compile — shows up here as a
    // frame with no world in it.
    let Some(gpu) = gpu() else { return };
    let chunks = scene();
    let mut renderer = prepare(gpu, &chunks, RenderMode::Textured);
    renderer.set_lighting_mode(LightingMode::Beautiful);
    let target = Offscreen::new(renderer.gpu(), WIDTH, HEIGHT);

    let frame = target
        .capture(&mut renderer, &viewpoint())
        .expect("capture");

    let top = average(&frame, 0, 0, WIDTH, HEIGHT / 8);
    let bottom = average(&frame, 0, HEIGHT * 3 / 4, WIDTH, HEIGHT);
    assert!(is_sky(top), "mode 3 lost the sky: {top:?}");
    assert!(!is_sky(bottom), "mode 3 lost the world: {bottom:?}");

    // The tonemap is not a no-op, and this is the cheapest honest way to say
    // so: the same renderer, the same uploaded meshes, the same viewpoint, and
    // mode 2 goes straight to the target instead. Identical frames would mean
    // the chain ran and changed nothing.
    renderer.set_lighting_mode(LightingMode::Classic);
    let plain = target
        .capture(&mut renderer, &viewpoint())
        .expect("capture");
    assert_ne!(
        perceptual_hash(&frame),
        perceptual_hash(&plain),
        "mode 3 produced the same picture as mode 2, so the post chain ran and changed nothing"
    );
}

#[test]
fn only_mode_three_allocates_the_post_chain() {
    // Task 10's criterion that mode 1 keeps Task 08's cost profile, "no
    // shadow/post allocations when in mode 1". Asserted as a property of the
    // renderer rather than measured as a frame time, for the reason the buffer
    // pool test gives: on lavapipe a texture is a malloc and the cost of one
    // measures nothing, while the count is the same on every driver.
    let Some(gpu) = gpu() else { return };
    let chunks = scene();
    let mut renderer = prepare(gpu, &chunks, RenderMode::Textured);
    let target = Offscreen::new(renderer.gpu(), WIDTH, HEIGHT);

    for mode in [LightingMode::Simple, LightingMode::Classic] {
        renderer.set_lighting_mode(mode);
        let _ = target
            .capture(&mut renderer, &viewpoint())
            .expect("capture");
        assert_eq!(
            renderer.post_bytes(),
            0,
            "{mode:?} allocated post targets it cannot draw with"
        );
    }

    renderer.set_lighting_mode(LightingMode::Beautiful);
    let _ = target
        .capture(&mut renderer, &viewpoint())
        .expect("capture");
    assert!(
        renderer.post_bytes() > 0,
        "mode 3 drew without allocating anything, so it is not running the chain"
    );

    // And gives it back. A player who tries mode 3 once should not still be
    // paying for its targets after going back for the frame rate.
    renderer.set_lighting_mode(LightingMode::Simple);
    assert_eq!(
        renderer.post_bytes(),
        0,
        "leaving mode 3 kept its targets alive"
    );
}

#[test]
fn a_surface_brighter_than_white_bleeds_light_past_its_edge() {
    // Bloom, as the only thing it can honestly be asserted to be: light where
    // the geometry is not. A lamp-lit surface in mode 3 is pushed past white by
    // `EMISSIVE_GAIN`, the threshold catches it, and the blur spreads it — so
    // the sky just above the world gets brighter than the same sky in mode 2.
    //
    // Without something over white nothing blooms at all, which is why this
    // scene lights its blocks rather than reusing the daylit fixture: the rest
    // of the renderer lands in 0..1 by construction.
    let Some(gpu) = gpu() else { return };

    struct Lamplit;
    impl client::shade::BlockLight for Lamplit {
        fn at(&self, _x: i32, _y: i32, _z: i32) -> tiamot_core::light::Light {
            // Full block light, no sun: a room lit entirely by lamps.
            tiamot_core::light::Light::new(0, 15, 15, 15)
        }
    }

    let chunks = scene();
    let mut renderer = Renderer::new(gpu, RenderMode::Textured, WIDTH, HEIGHT).expect("renderer");
    renderer.set_atlas(&Atlas::build(&[
        None,
        None,
        Some(Image::white_with_border()),
    ]));
    let by_pos: std::collections::BTreeMap<ChunkPos, &Chunk> =
        chunks.iter().map(|chunk| (chunk.pos(), chunk)).collect();
    for chunk in &chunks {
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
        renderer.set_chunk(
            pos,
            &mesher::mesh_chunk(chunk, &neighbours, Absent::Air, &Lamplit),
        );
    }

    let target = Offscreen::new(renderer.gpu(), WIDTH, HEIGHT);

    // The band of sky immediately above the horizon, which is where light
    // spilling off the world has to land.
    let horizon = |frame: &Image| {
        let mut brightest = 0.0f32;
        for y in 0..HEIGHT {
            let row = average(frame, 0, y, WIDTH, y + 1);
            if is_sky(row) {
                brightest = brightest.max(row[0] + row[1] + row[2]);
            }
        }
        brightest
    };

    renderer.set_lighting_mode(LightingMode::Classic);
    let plain = target
        .capture(&mut renderer, &viewpoint())
        .expect("capture");
    renderer.set_lighting_mode(LightingMode::Beautiful);
    let bloomed = target
        .capture(&mut renderer, &viewpoint())
        .expect("capture");

    let before = horizon(&plain);
    let after = horizon(&bloomed);
    assert!(
        after > before,
        "the brightest sky is {after} in mode 3 against {before} in mode 2, so nothing bled \
         past the geometry and the bloom passes did nothing"
    );
}

#[test]
fn a_low_sun_casts_longer_shadows_than_a_high_one() {
    // The cascades, as pixels. The fixed scene has a block standing proud of
    // its floor, so moving the sun down moves that block's shadow out across
    // the floor and the floor gets darker overall.
    //
    // Compared against the SAME mode with the sun somewhere else, deliberately.
    // Comparing mode 3 against mode 2 would also fold in the highlight
    // shoulder, which darkens the brightest surfaces whether or not anything is
    // shadowing them — a test that would pass with the shadow pass deleted.
    let Some(gpu) = gpu() else { return };
    let chunks = scene();
    let mut renderer = prepare(gpu, &chunks, RenderMode::Textured);
    renderer.set_lighting_mode(LightingMode::Beautiful);
    let target = Offscreen::new(renderer.gpu(), WIDTH, HEIGHT);

    // Nearly overhead: a block's shadow is under the block.
    renderer.set_sun(1.0, [1.0, 1.0, 1.0], [0.05, -0.99, 0.1]);
    let overhead = target
        .capture(&mut renderer, &viewpoint())
        .expect("capture");

    // Low and to one side: the same block throws its shadow across the floor.
    renderer.set_sun(1.0, [1.0, 1.0, 1.0], [0.75, -0.35, 0.55]);
    let low = target
        .capture(&mut renderer, &viewpoint())
        .expect("capture");

    // The floor, which is the bottom of the frame — see `viewpoint`.
    let floor = |frame: &Image| {
        let colour = average(frame, 0, HEIGHT / 2, WIDTH, HEIGHT);
        (colour[0] + colour[1] + colour[2]) / 3.0
    };

    let bright = floor(&overhead);
    let shadowed = floor(&low);
    assert!(
        shadowed < bright - 0.01,
        "the floor is {shadowed} under a low sun against {bright} under a high one, so nothing \
         was shadowed by moving the sun"
    );
}

/// The fixed scene with a stone canopy floating over the middle chunk.
///
/// Somewhere to stand with nothing but an underside overhead, which the fixed
/// scene has nowhere: its only downward faces are the rim of one hole, too few
/// pixels to average honestly.
fn canopy_scene() -> Vec<Chunk> {
    let mut chunks = scene_at(ChunkPos::new(0, 0, 0));
    for chunk in &mut chunks {
        let pos = chunk.pos();
        if (pos.x, pos.z) != (1, 1) {
            continue;
        }
        let corner = BlockPos::from_chunk_corner(pos);
        for x in 0..16 {
            for z in 0..16 {
                chunk
                    .set_block(
                        BlockPos::new(corner.x + x, corner.y + 12, corner.z + z),
                        BlockValue::Uniform(STONE),
                    )
                    .expect("in chunk");
            }
        }
    }
    chunks
}

#[test]
fn the_underside_of_an_overhang_is_not_lit_as_though_the_sun_were_under_it() {
    // **Reported from the window: "the underside of blocks, when lit at all,
    // are completely lit, which is odd."** It was, and the reason was that the
    // depth pass culled front faces — so the depth recorded for an underside
    // was the underside, which compares equal to itself and passes every test.
    // A surface the sun is behind is in shadow because of where it points, and
    // no depth map can say otherwise.
    //
    // The stored light is full daylight everywhere in this scene, deliberately:
    // that removes the server's propagated light from the question and leaves
    // only what the shader does with a face's direction.
    let Some(gpu) = gpu() else { return };
    let chunks = canopy_scene();
    let mut renderer = prepare(gpu, &chunks, RenderMode::Textured);
    let target = Offscreen::new(renderer.gpu(), WIDTH, HEIGHT);

    // Under the canopy at y = 12, above the floor at y = 8, looking up.
    let mut below = Camera {
        position: Position::from_world(24.0, 10.0, 24.0),
        ..Camera::default()
    };
    below.look(0.0, 1.3);

    // And the floor from above, as the thing to measure the underside against.
    let above = viewpoint();

    // Nearly overhead, so the canopy's underside is as far from the sun as a
    // face can be and the floor's top is square on to it.
    let sun = [0.05_f32, -0.99, 0.1];

    let luminance = |frame: &Image| {
        let colour = average(frame, WIDTH / 4, HEIGHT / 4, WIDTH * 3 / 4, HEIGHT * 3 / 4);
        (colour[0] + colour[1] + colour[2]) / 3.0
    };

    let ratio = |renderer: &mut Renderer| {
        renderer.set_sun(1.0, [1.0, 1.0, 1.0], sun);
        let under = luminance(&target.capture(renderer, &below).expect("capture"));
        let top = luminance(&target.capture(renderer, &above).expect("capture"));
        under / top
    };

    // Mode 2 has no shadow map, so its underside keeps `face_shade`'s half and
    // nothing else. This is the counter-example that makes the assertion below
    // non-vacuous: without it, a test that mode 3's underside is dark would
    // also pass if every mode had always drawn it dark.
    renderer.set_lighting_mode(LightingMode::Classic);
    let classic = ratio(&mut renderer);

    renderer.set_lighting_mode(LightingMode::Beautiful);
    let beautiful = ratio(&mut renderer);

    assert!(
        beautiful < classic * 0.75,
        "an overhang's underside is {beautiful} of the floor's brightness in mode 3 and \
         {classic} in mode 2 — the sun is still reaching a face that points away from it"
    );
}

#[test]
fn a_face_the_sun_is_behind_is_no_brighter_than_the_shadow_it_casts() {
    // **Reported from the window: shadows "fall just short of the corner
    // between two blocks and the light bleeds".**
    //
    // The bleed was not the shadow being short. It was the caster's own
    // shadowed side being fully lit: with front faces culled in the depth pass,
    // the depth recorded for a face pointing away from the sun was that face
    // itself, so it compared equal to its own depth and passed. A block under a
    // low sun therefore had a bright north side sitting directly on top of the
    // dark ground it was shadowing, and the corner between them read as light
    // getting in where it could not.
    //
    // Measured, on this scene, before and after: the north face went from 0.75
    // against a 0.14 shadow — five times brighter than the shadow it was
    // casting — to 0.52 against 0.56, which is a corner rather than a seam.
    let Some(gpu) = gpu() else { return };
    let chunks = scene();
    let mut renderer = prepare(gpu, &chunks, RenderMode::Textured);
    renderer.set_lighting_mode(LightingMode::Beautiful);
    let target = Offscreen::new(renderer.gpu(), WIDTH, HEIGHT);

    // The block standing proud of the floor at (20, 8, 20), close up, with the
    // sun low and to the south so it throws its shadow north toward the camera
    // rather than behind itself where nothing could see it.
    let mut camera = Camera {
        position: Position::from_world(20.5, 11.0, 25.0),
        ..Camera::default()
    };
    camera.look(std::f32::consts::PI, -0.62);
    renderer.set_sun(1.0, [1.0, 1.0, 1.0], [0.0, -0.45, 0.89]);
    let frame = target.capture(&mut renderer, &camera).expect("capture");

    let luminance = |x0, y0, x1, y1| {
        let colour = average(&frame, x0, y0, x1, y1);
        (colour[0] + colour[1] + colour[2]) / 3.0
    };

    // Windows well inside each region rather than at its edge, so that a
    // driver's rasterisation landing half a pixel elsewhere cannot move them
    // onto the boundary between the two.
    let face = luminance(150, 92, 176, 112);
    let shadow = luminance(150, 130, 176, 165);
    let floor = luminance(40, 150, 90, 190);

    assert!(
        face < shadow * 1.6,
        "the block's shadowed side reads {face} above a shadow of {shadow} — the sun is \
         reaching a face it stands behind, which is what puts a bright seam in the corner"
    );
    // Both of them really are in shadow, and this is really mode 3. Without
    // this the assertion above would also hold on a frame where nothing was
    // shadowed at all and every reading was the same bright number.
    assert!(
        shadow < floor * 0.8,
        "the shadowed ground reads {shadow} against open floor at {floor}, so there is no \
         shadow here to have been continuous with"
    );
}

#[test]
fn a_sealed_room_does_not_get_brighter_when_the_sky_does() {
    // **Reported from the window: "when in a cave as day comes the ambient
    // light makes even a completely enclosed space brighter."**
    //
    // The floor under the darkest place was `sky_colour * AMBIENT_FLOOR`, and a
    // sky colour carries the sky's brightness as well as its hue — so the floor
    // rose and fell with the sun in a room the sun could not reach. Charter
    // rule 19 puts that decision in the stored sunlight channel, which the
    // server had already worked out to be zero down there.
    //
    // Built with a stored light of zero everywhere rather than with geometry
    // that encloses something: what is under test is the shader's floor, and
    // zero is zero however a room came by it.
    let Some(gpu) = gpu() else { return };
    let chunks = scene();
    let mut renderer = prepare(gpu, &chunks, RenderMode::Textured);
    renderer.set_lighting_mode(LightingMode::Classic);
    upload_lit(
        &mut renderer,
        &chunks,
        &client::shade::Uniform(tiamot_core::light::Light::DARK),
    );
    let target = Offscreen::new(renderer.gpu(), WIDTH, HEIGHT);

    // Fog pushed far beyond the scene. Left where it is, `set_sky` would mix
    // the sky's own colour into every surface and the test would measure the
    // fog rather than the floor.
    let sunless = |renderer: &mut Renderer, sky: [f32; 3]| {
        renderer.set_sky(sky, 10_000.0);
        renderer.set_sun(1.0, [1.0, 1.0, 1.0], [0.05, -0.99, 0.1]);
        let frame = target.capture(renderer, &viewpoint()).expect("capture");
        // The bottom of the frame is the floor — see `viewpoint`.
        let colour = average(&frame, 0, HEIGHT * 3 / 4, WIDTH, HEIGHT);
        (colour[0] + colour[1] + colour[2]) / 3.0
    };

    let midnight = sunless(&mut renderer, [0.02, 0.03, 0.06]);
    let noon = sunless(&mut renderer, [0.53, 0.81, 0.92]);

    assert!(
        noon < midnight * 1.25,
        "an unlit floor reads {noon} under a bright sky and {midnight} under a dark one, so the \
         sky is lighting a place its light never reached"
    );
    // And the floor is genuinely being drawn at the ambient level rather than
    // black or missing, which is the reading that would make the comparison
    // above true by having nothing in it.
    assert!(
        midnight > 0.01,
        "the unlit floor reads {midnight}, which is not a floor anyone could see by"
    );
}

#[test]
fn mode_three_tints_its_fog_toward_the_sun() {
    // Depth fog in the post chain, and the reason it is there rather than in
    // the world shader: it reaches the SKY, which has no geometry and therefore
    // no per-surface fog to apply. The haze around the sun is what that buys.
    let Some(gpu) = gpu() else { return };
    let chunks = scene();
    let mut renderer = prepare(gpu, &chunks, RenderMode::Textured);
    renderer.set_lighting_mode(LightingMode::Beautiful);
    // Fog close in, so the whole frame is hazy rather than only its horizon.
    renderer.set_sky(client::render::sky_colour(), 60.0);
    let target = Offscreen::new(renderer.gpu(), WIDTH, HEIGHT);

    // Looking level, so the frame is mostly sky.
    let mut camera = Camera {
        position: Position::from_world(24.0, 12.0, 20.0),
        ..Camera::default()
    };
    camera.look(0.0, 0.0);

    // A strongly coloured sun, straight ahead and low: the camera looks along
    // +z at yaw 0, and light travelling toward -z is light coming from in front.
    let orange = [1.0, 0.45, 0.1];
    renderer.set_sun(1.0, orange, [0.0, -0.3, -0.954]);
    let toward = target.capture(&mut renderer, &camera).expect("capture");

    // The same sun, behind. Nothing else changes, so any difference in the sky
    // is the scattering term and not the sun's colour reaching a surface.
    renderer.set_sun(1.0, orange, [0.0, -0.3, 0.954]);
    let away = target.capture(&mut renderer, &camera).expect("capture");

    // The middle of the sky, where the sun is when it is in front.
    let warmth = |frame: &Image| {
        let colour = average(frame, WIDTH / 3, 0, WIDTH * 2 / 3, HEIGHT / 3);
        colour[0] - colour[2]
    };

    assert!(
        warmth(&toward) > warmth(&away) + 0.02,
        "the sky is {} facing the sun and {} facing away, so the haze is not taking the sun's \
         colour",
        warmth(&toward),
        warmth(&away)
    );
}

#[test]
fn an_occluded_corner_loses_its_colour_rather_than_keeping_it_dimly() {
    // Reported twice from the window as "the AO is yellow", and the second time
    // after a commit that claimed to have fixed it — because that fix moved a
    // multiply from one side of an associative product to the other, which is
    // no change at all. A scaled colour keeps its hue exactly, so a corner
    // beside a warm lamp stays warm and only gets darker.
    //
    // The property that says it is fixed: an occluded corner under coloured
    // light is LESS SATURATED than an open surface under the same light, not
    // merely dimmer.
    let Some(gpu) = gpu() else { return };

    /// Full warm block light, no sun: a world lit only by lamps, which is where
    /// the complaint comes from.
    struct WarmLamps;
    impl client::shade::BlockLight for WarmLamps {
        fn at(&self, _x: i32, _y: i32, _z: i32) -> tiamot_core::light::Light {
            tiamot_core::light::Light::new(0, 15, 11, 6)
        }
    }

    let chunks = scene();
    let mut renderer = prepare(gpu, &chunks, RenderMode::Textured);
    renderer.set_lighting_mode(LightingMode::Classic);
    let by_pos: std::collections::BTreeMap<ChunkPos, &Chunk> =
        chunks.iter().map(|chunk| (chunk.pos(), chunk)).collect();
    for chunk in &chunks {
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
        renderer.set_chunk(
            pos,
            &mesher::mesh_chunk(chunk, &neighbours, Absent::Air, &WarmLamps),
        );
    }

    let target = Offscreen::new(renderer.gpu(), WIDTH, HEIGHT);
    let frame = target
        .capture(&mut renderer, &viewpoint())
        .expect("capture");

    // Saturation as the gap between the strongest and weakest channel, scaled
    // by brightness — the thing "yellow" describes.
    let saturation = |colour: [f32; 3]| {
        let high = colour[0].max(colour[1]).max(colour[2]);
        let low = colour[0].min(colour[1]).min(colour[2]);
        if high <= f32::EPSILON {
            0.0
        } else {
            (high - low) / high
        }
    };

    // The brightest and darkest patches in the frame: open surface against
    // occluded corner, under the same lamp colour.
    let mut brightest = ([0.0; 3], -1.0f32);
    let mut darkest = ([0.0; 3], f32::MAX);
    for y in (0..HEIGHT - 8).step_by(8) {
        for x in (0..WIDTH - 8).step_by(8) {
            let colour = average(&frame, x, y, x + 8, y + 8);
            if is_sky(colour) {
                continue;
            }
            let level = colour[0] + colour[1] + colour[2];
            if level > brightest.1 {
                brightest = (colour, level);
            }
            if level < darkest.1 {
                darkest = (colour, level);
            }
        }
    }

    assert!(
        darkest.1 < brightest.1,
        "the frame has no shading in it at all: {darkest:?} against {brightest:?}"
    );
    assert!(
        saturation(darkest.0) < saturation(brightest.0),
        "the darkest patch is {} saturated against the brightest's {} — occlusion is scaling \
         the colour rather than taking it away, which is what reads as yellow",
        saturation(darkest.0),
        saturation(brightest.0)
    );
}

#[test]
fn the_debug_body_is_actually_drawn_and_actually_casts() {
    // Reported from the window as "I am not seeing any shadow on me", which has
    // two possible causes and they need telling apart: the box is not being
    // drawn at all, or it is drawn and does not reach the shadow map.
    //
    // Both are checked here from a fixed camera, so nothing depends on where a
    // predicted body happened to be.
    let Some(gpu) = gpu() else { return };
    let chunks = scene();
    let mut renderer = prepare(gpu, &chunks, RenderMode::Textured);
    let target = Offscreen::new(renderer.gpu(), WIDTH, HEIGHT);

    // Camera-relative blocks, which is the only coordinate system the renderer
    // has (charter rule 7). `viewpoint` looks down and forward along +z from
    // ten blocks above the floor, so this is out in front of it and on the
    // ground — an earlier version put the body six blocks ABOVE a camera
    // pointing down, and "the body is not in the frame" was the test's fault
    // rather than the renderer's.
    let where_it_stands = [0.0, -10.0, 6.0];

    renderer.set_lighting_mode(LightingMode::Classic);
    let without = target
        .capture(&mut renderer, &viewpoint())
        .expect("capture");
    renderer.set_body(Some(where_it_stands));
    let with = target
        .capture(&mut renderer, &viewpoint())
        .expect("capture");
    // Counted pixels rather than the perceptual hash, and the difference
    // matters: the body is two cells wide and five tall, which is about forty
    // pixels at this resolution, and the hash averages the frame into a 16x16
    // grid precisely so that something that small cannot move it. The hash is
    // the right tool for "did the world stop drawing" and the wrong one for
    // "is this one small object present".
    let differing = (0..HEIGHT)
        .step_by(2)
        .flat_map(|y| (0..WIDTH).step_by(2).map(move |x| (x, y)))
        .filter(|(x, y)| without.pixel(*x, *y) != with.pixel(*x, *y))
        .count();
    assert!(
        differing > 8,
        "only {differing} sampled pixels changed when the body appeared, so third person shows          the world moving around nothing"
    );

    // And in mode 3, where it has to reach the cascades as well. The sun is put
    // low and to one side so the box throws a shadow across the floor rather
    // than under itself.
    renderer.set_lighting_mode(LightingMode::Beautiful);
    renderer.set_sun(1.0, [1.0, 1.0, 1.0], [0.75, -0.35, 0.55]);
    renderer.set_body(None);
    let unshadowed = target
        .capture(&mut renderer, &viewpoint())
        .expect("capture");
    renderer.set_body(Some(where_it_stands));
    let shadowed = target
        .capture(&mut renderer, &viewpoint())
        .expect("capture");

    // The floor is darker with the body there: its own pixels plus the shadow
    // it throws. Measured over the whole frame so it does not depend on knowing
    // where the shadow lands.
    let ground = |frame: &Image| {
        let colour = average(frame, 0, HEIGHT / 2, WIDTH, HEIGHT);
        colour[0] + colour[1] + colour[2]
    };
    assert!(
        ground(&shadowed) < ground(&unshadowed),
        "the floor is {} with the body and {} without it, so the body reaches the world pass \
         but not the shadow pass",
        ground(&shadowed),
        ground(&unshadowed)
    );
}

#[test]
fn shadow_quality_changes_what_is_allocated_and_off_allocates_nothing() {
    // Four settings because the cascades are the largest textures the client
    // allocates and the right size depends entirely on the card. Asserted as
    // memory rather than as sharpness: how sharp a shadow looks is a human
    // gate, how much it costs is not.
    let Some(gpu) = gpu() else { return };
    let chunks = scene();
    let mut renderer = prepare(gpu, &chunks, RenderMode::Textured);
    renderer.set_lighting_mode(LightingMode::Beautiful);
    let target = Offscreen::new(renderer.gpu(), WIDTH, HEIGHT);

    let mut previous = 0;
    for quality in [
        ShadowQuality::Off,
        ShadowQuality::Low,
        ShadowQuality::Medium,
        ShadowQuality::High,
    ] {
        renderer.set_shadow_quality(quality);
        let frame = target
            .capture(&mut renderer, &viewpoint())
            .expect("capture");
        let ground = average(&frame, 0, HEIGHT * 3 / 4, WIDTH, HEIGHT);
        assert!(
            !is_sky(ground),
            "{quality:?} drew no world at all, so the setting broke the frame rather than \
             changing its shadows"
        );

        let bytes = renderer.post_bytes();
        match quality {
            ShadowQuality::Off => {
                // The rest of mode 3 is still there — the float target and the
                // bloom buffers — so this is not zero, only smaller than any
                // setting that allocates cascades.
                previous = bytes;
            }
            _ => {
                assert!(
                    bytes > previous,
                    "{quality:?} allocated {bytes} against the previous setting's {previous}, so \
                     the ladder does not climb"
                );
                previous = bytes;
            }
        }
    }

    // And back to off gives it all back.
    renderer.set_shadow_quality(ShadowQuality::Off);
    let _ = target
        .capture(&mut renderer, &viewpoint())
        .expect("capture");
    assert!(
        renderer.post_bytes() < previous,
        "turning shadows off kept their memory"
    );
}

#[test]
fn the_grading_table_leaves_an_ungraded_frame_exactly_alone() {
    // **The test that catches the classic LUT bug.** A 3D lookup table is
    // addressed by texture coordinate, and a coordinate of 0 lands on the EDGE
    // of the first texel rather than its centre — so the obvious mapping samples
    // half a texel outside the table at both ends and clamps there, which reads
    // as slightly crushed blacks and whites that no amount of staring at the
    // grade explains. An identity grade run through the table is the one case
    // where that error is unambiguous: any difference from the ungraded frame is
    // the mapping, because the grade itself is doing nothing.
    let Some(gpu) = gpu() else { return };
    let chunks = scene();
    let mut renderer = prepare(gpu, &chunks, RenderMode::Textured);
    renderer.set_lighting_mode(LightingMode::Beautiful);
    let target = Offscreen::new(renderer.gpu(), WIDTH, HEIGHT);

    // `SkyGrade::NONE` skips the table entirely, so it cannot be what proves the
    // mapping. A grade that is the identity in every field EXCEPT one set to a
    // value that changes nothing — a gamma of exactly 1 is the identity, but it
    // is not `NONE` and therefore does go through the table.
    let ungraded = target
        .capture(&mut renderer, &viewpoint())
        .expect("capture");
    renderer.set_grade(SkyGrade {
        // Off the identity by less than a quantisation step, so the table is
        // built and sampled while grading nothing anyone could see.
        exposure: 1.0 + 1e-7,
        ..SkyGrade::NONE
    });
    let through_the_table = target
        .capture(&mut renderer, &viewpoint())
        .expect("capture");

    // Every sampled region has to agree, not merely the average: the mapping
    // error is largest at the ends of the range, so a whole-frame mean would
    // cancel a crushed black against a crushed white.
    for (y0, y1) in [
        (0, HEIGHT / 4),
        (HEIGHT / 4, HEIGHT / 2),
        (HEIGHT / 2, HEIGHT),
    ] {
        let before = average(&ungraded, 0, y0, WIDTH, y1);
        let after = average(&through_the_table, 0, y0, WIDTH, y1);
        for channel in 0..3 {
            let drift = (before[channel] - after[channel]).abs();
            assert!(
                drift < 0.01,
                "rows {y0}..{y1} channel {channel} moved by {drift} ({} to {}) through a table \
                 that grades nothing — the lookup coordinates are wrong",
                before[channel],
                after[channel]
            );
        }
    }
}

#[test]
fn a_graded_sky_changes_the_picture_in_the_direction_it_asked_for() {
    // Grading, as pixels: the six knobs reach the frame, and reach it the right
    // way round. Asserted per knob rather than as one hash, because a table with
    // its channels transposed or its contrast inverted would still change the
    // picture — and "the picture changed" is what a hash can tell you.
    let Some(gpu) = gpu() else { return };
    let chunks = scene();
    let mut renderer = prepare(gpu, &chunks, RenderMode::Textured);
    renderer.set_lighting_mode(LightingMode::Beautiful);
    let target = Offscreen::new(renderer.gpu(), WIDTH, HEIGHT);

    // The ground, which is lit and mid-bright — the part of the frame every knob
    // has room to move in either direction.
    let ground = |renderer: &mut Renderer| {
        let frame = target.capture(renderer, &viewpoint()).expect("capture");
        average(&frame, 0, HEIGHT * 3 / 4, WIDTH, HEIGHT)
    };
    let plain = ground(&mut renderer);

    // A blue tint must move blue up and leave red where it was.
    renderer.set_grade(SkyGrade {
        tint: [1.0, 1.0, 1.4],
        ..SkyGrade::NONE
    });
    let tinted = ground(&mut renderer);
    assert!(
        tinted[2] > plain[2] + 0.02,
        "a blue tint left blue at {} against {}",
        tinted[2],
        plain[2]
    );
    assert!(
        (tinted[0] - plain[0]).abs() < 0.02,
        "a blue tint moved red from {} to {}, so the table's axes are transposed",
        plain[0],
        tinted[0]
    );

    // Greyscale must land every channel on the same value.
    renderer.set_grade(SkyGrade {
        saturation: 0.0,
        ..SkyGrade::NONE
    });
    let grey = ground(&mut renderer);
    assert!(
        (grey[0] - grey[2]).abs() < 0.02,
        "saturation 0 left {grey:?}, which is not grey"
    );

    // And gamma must lift the midtones without touching the ends. The ground is
    // brighter than mid grey here, so a gamma above 1 raises it.
    renderer.set_grade(SkyGrade {
        gamma: 2.0,
        ..SkyGrade::NONE
    });
    let lifted = ground(&mut renderer);
    assert!(
        lifted[1] > plain[1] + 0.02,
        "a gamma of 2 left the ground at {} against {}",
        lifted[1],
        plain[1]
    );
}

#[test]
fn a_still_sky_bakes_its_grading_table_once() {
    // The table is 4,096 entries and every one of them costs a `powf` for the
    // sRGB encode. Re-baking it per frame for a sky that moves by a millionth
    // would be a per-frame cost for no per-frame difference — so the bake is
    // gated on the grade having moved far enough to reach a pixel, and this is
    // the assertion that the gate is real.
    let Some(gpu) = gpu() else { return };
    let gpu = std::sync::Arc::new(gpu);
    let mut grading = client::render::grade::Grading::new(&gpu);

    let grade = SkyGrade {
        saturation: 0.8,
        ..SkyGrade::NONE
    };
    assert!(
        grading.bake(&gpu, &grade),
        "the first bake has to happen; there is nothing in the texture yet"
    );
    assert!(!grading.bake(&gpu, &grade), "the same grade baked twice");

    // A change too small to survive eight bits is not a reason to re-bake.
    let imperceptible = SkyGrade {
        saturation: 0.8 + 1.0 / 8192.0,
        ..SkyGrade::NONE
    };
    assert!(!grading.bake(&gpu, &imperceptible));

    // One that can reach a pixel is.
    let visible = SkyGrade {
        saturation: 0.6,
        ..SkyGrade::NONE
    };
    assert!(
        grading.bake(&gpu, &visible),
        "a grade that moved by 0.2 did not re-bake"
    );
}
