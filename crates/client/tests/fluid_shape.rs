// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The shape of a body of milk: what it must NOT draw inside itself, and where
//! its surface has to meet itself.
//!
//! Reported from the window: "some of the internal faces of the water are
//! rendered especially when it falls and splats against the ground, the
//! transition between the fall and the ground splat. That transition is also
//! pretty harsh and could use some smoothing."
//!
//! Both were the same cause. Occupancy was filled to a whole number of sub-node
//! cells, so two adjacent blocks of milk at different levels occupied different
//! numbers of cells and face culling put a WALL between them — a pond came out
//! as a ziggurat of terraces, and the back side of every terrace's step showed
//! through the transparent surface in front of it. A chunk seam did the same
//! thing for a different reason: the padding carried terrain and not fluid, so
//! one body of milk drew a full wall of faces down every boundary it crossed,
//! twice over.
//!
//! These are the invariants that say it is fixed, written as properties of the
//! mesh rather than of any picture: a face between two wet blocks does not
//! exist, and a lattice position has exactly one height.

use client::camera::{Camera, Position};
use client::config::RenderMode;
use client::mesher::{self, Absent, FluidVertex, Mesh, Neighbours};
use client::render::{Gpu, Offscreen, Renderer};
use client::texture::{Atlas, Image};
use std::collections::HashMap;
use tiamot_core::{BlockPos, BlockValue, Chunk, ChunkPos, MaterialId};

const DAY: client::shade::Uniform = client::shade::Uniform(tiamot_core::light::Light::DAYLIGHT);
const WIDTH: u32 = 640;
const HEIGHT: u32 = 480;
const STONE: MaterialId = MaterialId(2);
const MILK: MaterialId = MaterialId(3);

/// The top of the ground, and the block row the pool spreads across.
const FLOOR: i32 = 3;
const POOL: i32 = 4;

fn gpu() -> Option<Gpu> {
    match Gpu::headless() {
        Ok(gpu) => Some(gpu),
        Err(err) => {
            assert!(
                std::env::var("TIAMOT_REQUIRE_GPU").is_err(),
                "TIAMOT_REQUIRE_GPU is set and no adapter was available: {err}"
            );
            println!("SKIPPING: no graphics adapter on this machine ({err})");
            None
        }
    }
}

/// The ground the milk lands on.
fn floor() -> Chunk {
    let mut chunk = Chunk::new(ChunkPos::new(0, 0, 0), MaterialId::AIR);
    for x in 0..16 {
        for z in 0..16 {
            for y in 0..=FLOOR {
                chunk
                    .set_block(BlockPos::new(x, y, z), BlockValue::Uniform(STONE))
                    .expect("in chunk");
            }
        }
    }
    chunk
}

/// A fall landing in a spreading pool, in fluid LEVELS.
///
/// The levels step down with distance because that is what a solver leaves
/// behind, and a uniform pool would exercise none of this: every artifact here
/// lived on the boundary between two blocks holding different amounts.
fn level_at(x: i32, y: i32, z: i32) -> u8 {
    // The falling column, one block wide, from the ceiling down to the pool.
    if x == 8 && z == 8 && (POOL + 1..=14).contains(&y) {
        return 1;
    }
    if y != POOL {
        return 0;
    }
    match (x - 8).abs().max((z - 8).abs()) {
        0 => 7,
        1 => 6,
        2 => 5,
        3 => 4,
        4 => 3,
        5 => 2,
        6 => 1,
        _ => 0,
    }
}

/// Whether a block holds any milk at all.
fn wet(x: i32, y: i32, z: i32) -> bool {
    level_at(x, y, z) > 0
}

/// What the client feeds the mesher, including the rule that a block with fluid
/// above it draws full — which is where a fall's shape comes from.
struct Splat;

impl mesher::FluidFill for Splat {
    fn fill(&self, x: i32, y: i32, z: i32) -> Option<(u16, u8)> {
        let level = level_at(x, y, z);
        if level == 0 {
            return None;
        }
        if wet(x, y + 1, z) {
            return Some((MILK.get(), 27));
        }
        // `Fluid::depth_units`, which is what the real table holds.
        Some((
            MILK.get(),
            u8::try_from(u32::from(level) * 24 / 7).unwrap_or(24),
        ))
    }
}

/// Milk in every block there is, so a face can only come from a seam.
struct Everywhere;

impl mesher::FluidFill for Everywhere {
    fn fill(&self, _x: i32, _y: i32, _z: i32) -> Option<(u16, u8)> {
        Some((MILK.get(), 27))
    }
}

fn splat() -> Mesh {
    mesher::mesh_chunk(&floor(), &Neighbours::none(), Absent::Solid, &DAY, &Splat)
}

/// A fluid quad, as the mesh stores it: four vertices in winding order.
fn quads(mesh: &Mesh) -> impl Iterator<Item = &[FluidVertex]> {
    mesh.fluid_vertices.chunks_exact(4)
}

/// Which block a cell coordinate falls in.
fn block_of(cell: u32) -> i32 {
    (cell / tiamot_core::SUBNODES_PER_AXIS) as i32
}

/// Where a vertex really sits, in blocks.
fn height_of(vertex: &FluidVertex) -> f32 {
    let (_, y, _) = vertex.position();
    y as f32 / tiamot_core::SUBNODES_PER_AXIS as f32 - f32::from(vertex.drop()) / 48.0
}

#[test]
fn a_pond_draws_no_faces_inside_itself() {
    // **The terraces, and the step walls that were their edges.**
    //
    // A level 6 block filled three cells and a level 5 block two, so the mesher
    // saw an exposed cell face between them and drew a wall one cell tall in the
    // middle of a body of milk. Every ring of the splat had one. Their back
    // sides showed through the transparent surface in front: the "internal
    // faces" reported from the window.
    //
    // A wet block is filled on the lattice now whatever its level, so two of
    // them side by side are occupied identically and the face between them is
    // interior. The shape lives entirely in where the surface vertices sit.
    let mesh = splat();

    let mut interior = Vec::new();
    for quad in quads(&mesh) {
        let (axis, positive) = quad[0].face();
        // Only the vertical faces separate two blocks that are side by side; a
        // horizontal one is the surface or the floor.
        if axis == 1 {
            continue;
        }
        // **A quad's vertices sit ON the plane the face lies in**, whichever
        // way it points, so the two blocks it separates are always the one the
        // coordinate lands in and the one just before it. A coordinate that is
        // not on a block boundary means a face INSIDE a block, which only
        // terrain can produce and which separates no two blocks at all.
        let (x, y, z) = quad[0].position();
        let (by, span) = (block_of(y), tiamot_core::SUBNODES_PER_AXIS);
        let along = if axis == 0 { x } else { z };
        if along % span != 0 {
            continue;
        }
        let (near, far) = match axis {
            0 => (
                (block_of(x) - 1, by, block_of(z)),
                (block_of(x), by, block_of(z)),
            ),
            _ => (
                (block_of(x), by, block_of(z) - 1),
                (block_of(x), by, block_of(z)),
            ),
        };
        if wet(near.0, near.1, near.2) && wet(far.0, far.1, far.2) {
            interior.push((axis, positive, near, far));
        }
    }

    assert!(
        interior.is_empty(),
        "the pool draws {} faces between two blocks that both hold milk — the \
         first is {:?}. Those are the terrace walls, and a transparent surface \
         shows every one of them from behind.",
        interior.len(),
        interior.first()
    );
}

#[test]
fn one_body_of_milk_crossing_a_chunk_seam_draws_nothing_there() {
    // **The padding carried terrain and not fluid.**
    //
    // Face culling at a chunk's edge reads the neighbour through two padding
    // bits per column, and only the neighbour's TERRAIN was ever written into
    // them. So a chunk whose neighbour was solid milk saw air and drew a full
    // wall of fluid faces down the seam — and the neighbour drew its own, so a
    // pond had a double-thick sheet of milk standing inside it at every boundary
    // it crossed. A fall crossing a seam had the same disc through it every
    // sixteen blocks, which is why this was reported as milk falling.
    let chunk = Chunk::new(ChunkPos::new(0, 0, 0), MaterialId::AIR);
    let neighbour = Chunk::new(ChunkPos::new(1, 0, 0), MaterialId::AIR);
    let mut sides = [None; 6];
    sides[1] = Some(&neighbour);

    let mesh = mesher::mesh_chunk(
        &chunk,
        &Neighbours { sides },
        Absent::Solid,
        &DAY,
        &Everywhere,
    );

    let on_seam = quads(&mesh)
        .filter(|quad| {
            let (axis, positive) = quad[0].face();
            axis == 0 && positive && quad[0].position().0 == tiamot_core::CHUNK_SUBNODES
        })
        .count();
    assert_eq!(
        on_seam, 0,
        "milk on both sides of a seam still draws {on_seam} faces down it"
    );
}

#[test]
fn the_surface_has_one_height_everywhere_it_meets_itself() {
    // **What "smooth" has to mean before it can mean anything else.**
    //
    // The surface is a sheet of quads whose corners are pulled down to where the
    // milk really is. Two quads that meet share lattice positions, so the same
    // lattice position given two different drops is a crack in the sheet — and a
    // crack in a transparent surface is a window onto the faces behind it, which
    // is the artifact this file is about.
    //
    // Unlike the two tests above this one does not reproduce a reported bug —
    // this fixture passed it before the fix as well. It is here because the fix
    // moved ALL of a body of milk's shape into the drop, so a crack is now the
    // failure mode with nothing else holding the sheet together, and because the
    // invariant is cheap to state: the drop is a pure function of a vertex's own
    // position, and a vertex can no longer be asked to rise above the block it
    // belongs to.
    let mesh = splat();

    let mut seen: HashMap<(u32, u32, u32), u16> = HashMap::new();
    for vertex in &mesh.fluid_vertices {
        let at = vertex.position();
        if let Some(&drop) = seen.get(&at) {
            assert_eq!(
                drop,
                vertex.drop(),
                "the lattice point {at:?} is drawn at two heights, {drop} and \
                 {} — the surface has a crack there",
                vertex.drop()
            );
        }
        seen.insert(at, vertex.drop());
    }
    assert!(!seen.is_empty(), "the splat produced no fluid geometry");
}

#[test]
fn the_surface_reaches_the_depth_the_milk_actually_has() {
    // **The harsh transition, from the other end.**
    //
    // The surface used to be pulled down from whatever whole number of cells the
    // occupancy had been rounded up to, which meant it could never sit lower
    // than that block's own lattice floor — and where the corner average wanted
    // to be HIGHER than the block's ceiling it was clamped flat instead. The
    // rim of a pool came out at 0.27 of a block when its milk was 0.10 deep, and
    // the difference was made up by the step faces this file's first test is
    // about.
    //
    // A wet block is full on the lattice now, so the whole block's height is
    // available to the drop and the surface goes exactly where the height field
    // says. The shallowest rim of the splat holds level 1, which is 3 of 27
    // units, so its surface is 5 of the 48 fine units above the floor.
    let mesh = splat();

    let surface: Vec<f32> = quads(&mesh)
        .filter(|quad| quad[0].face() == (1, true))
        .flat_map(<[FluidVertex]>::iter)
        .filter(|vertex| block_of(vertex.position().1) == POOL + 1)
        .map(height_of)
        .collect();
    assert!(!surface.is_empty(), "the pool has no surface");

    let lowest = surface.iter().copied().fold(f32::MAX, f32::min);
    let highest = surface.iter().copied().fold(f32::MIN, f32::max);

    // **The claim is that the LATTICE is not deciding**, which is what the
    // original of this test measured by naming the rim block's own height. It
    // cannot name that any more, and for a good reason: a corner averages the
    // four blocks meeting at it, and since the shoreline rule landed a corner
    // with open air beside it comes down to the floor
    // (`a_shoreline_comes_down_to_the_floor_and_a_wall_does_not`). So the rim
    // block's corners are mixtures now, and no vertex sits at exactly the
    // height one level asks for.
    //
    // What still holds, and is the thing worth guarding: the surface takes
    // values BETWEEN the lattice's steps. A block is three cells, so a height
    // clamped to the lattice is a whole number of thirds; a height field is
    // free of it only if something lands off one.
    let off_lattice = surface
        .iter()
        .filter(|height| {
            // `floor_to_i32` rather than `round`: rounding is on charter rule
            // 4's banned list and a test takes no exemption from it, which is
            // the gate working. The fractional part is what matters here and
            // either gives it.
            let cells = (*height - POOL as f32) * 3.0;
            let fraction = cells - tiamot_core::detgen::floor_to_i32(cells) as f32;
            fraction > 1.0 / 48.0 && fraction < 1.0 - 1.0 / 48.0
        })
        .count();
    assert!(
        off_lattice > 0,
        "every one of the pool's {} surface heights is a whole number of cells, \
         so the lattice is still deciding where the surface can go",
        surface.len()
    );

    // And it reaches the floor at its outer rim rather than ending in a cliff,
    // which is the shoreline rule seen from here.
    assert!(
        lowest < POOL as f32 + 3.0 / 48.0,
        "the pool's outermost surface is at {lowest}, a floor of {POOL} away — \
         a shoreline that does not come down is the vertical edge a puddle was \
         reported as having"
    );
    assert!(
        lowest >= POOL as f32 - f32::EPSILON,
        "the surface came out at {lowest}, below the floor the pool sits on"
    );

    // And the deepest point is the block top, because the fall standing in it
    // is milk too and a surface does not exist under more milk.
    let brim = (POOL + 1) as f32;
    assert!(
        (highest - brim).abs() <= f32::EPSILON,
        "the pool's surface tops out at {highest} rather than {brim}, so it does \
         not reach the fall landing in it"
    );
}

#[test]
#[ignore = "debugging aid: writes PNGs rather than asserting anything"]
fn dump_the_splat() {
    let Some(gpu) = gpu() else { return };
    let mut renderer = Renderer::new(gpu, RenderMode::Textured, WIDTH, HEIGHT).expect("renderer");
    let atlas = Atlas::build(&[
        None,
        None,
        Some(Image::solid(16, 16, [120, 110, 100, 255])),
        Some(Image::solid(16, 16, [235, 240, 250, 255])),
    ]);
    renderer.set_atlas(&atlas);

    let mesh = splat();
    println!(
        "{} opaque quads, {} fluid quads",
        mesh.quads.len(),
        mesh.fluid_quad_count()
    );
    renderer.set_chunk(ChunkPos::new(0, 0, 0), &mesh);

    let target = Offscreen::new(renderer.gpu(), WIDTH, HEIGHT);
    let views: [(&str, [f64; 3], f32); 4] = [
        ("side", [8.0, 6.0, 2.0], -0.15),
        ("close", [8.0, 5.2, 4.5], -0.05),
        ("above", [8.0, 11.0, 2.0], -0.7),
        ("low", [8.0, 4.6, 3.0], 0.1),
    ];
    for (label, position, pitch) in views {
        let mut camera = Camera {
            position: Position::from_world(position[0], position[1], position[2]),
            ..Camera::default()
        };
        camera.look(0.0, pitch);
        let frame = target.capture(&mut renderer, &camera).expect("capture");
        let path = std::env::temp_dir().join(format!("tiamot-splat-{label}.png"));
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
    }
}

#[test]
fn a_shoreline_stands_no_wall_of_milk_under_its_own_surface() {
    // **Reported from a running game**, looking at the skirt: "there are still
    // full single quads that render at full height at the edges of the puddle,
    // like a big ring around the shoreline."
    //
    // A fluid side face is several quads stacked up a block, and only the
    // TOPMOST vertex was pulled down to the surface. Every quad below it stayed
    // pinned to the lattice, so the milk's cross-section never followed its own
    // surface down: a wall of milk standing under a surface that had already
    // come down. Inside a pond those faces are culled and it cannot be seen —
    // around the outside of one it is a ring.
    //
    // The skirt did not cause it, it made it plain: a skirt block's surface is
    // ON the floor, so the whole of its side face was left standing.
    //
    // Measured on this fixture: the tallest standing edge in a dry block was
    // **16 fine units**, a third of a block, and is now **one** — a forty-eighth,
    // which is integer rounding in the surface rather than geometry anybody
    // would see. The bound is that one unit, so the ring reappearing at a third
    // of a block fails by a factor of sixteen.
    let mesh = splat();
    let per_axis = tiamot_core::SUBNODES_PER_AXIS;
    let fine = 48 / per_axis;

    let tallest = mesh
        .fluid_vertices
        .iter()
        // Side faces only: a top face lying flat on the floor is the shoreline
        // working, not a wall.
        .filter(|vertex| vertex.face().0 != 1)
        .filter(|vertex| {
            let (x, _, z) = vertex.position();
            // Dry blocks only — the skirt ring and beyond.
            !wet((x / per_axis) as i32, POOL, (z / per_axis) as i32)
        })
        .map(|vertex| {
            let (_, y, _) = vertex.position();
            ((y % per_axis) * fine).saturating_sub(u32::from(vertex.drop()))
        })
        .max()
        .unwrap_or(0);

    assert!(
        tallest <= 1,
        "the shoreline leaves {tallest} fine units of milk standing in a dry \
         block whose surface is the floor — the ring of full-height quads \
         reported around a puddle"
    );
}
