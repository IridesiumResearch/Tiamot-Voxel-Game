// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

// The world shader: unpacks the 8-byte vertex, places it with the floating
// origin, and samples the atlas.
//
// TEXTURE COORDINATES ARE IN BLOCKS, NOT IN QUADS. Greedy meshing merges a flat
// surface into one quad that may span the whole 48-cell chunk, so a UV that ran
// 0..1 across the quad would stretch one texture over sixteen blocks. Instead
// the coordinate is the position along the face measured in blocks, and the
// fragment shader takes its fractional part: one repeat per block, whatever the
// quad's size.
//
// That fract() is also why sampling uses textureSampleGrad. The derivative of a
// wrapped coordinate is enormous at every block boundary, and an automatic
// mip selection would read it as "this surface is edge-on", drop to the
// smallest level, and draw a seam of average colour along every block edge.
// Passing the unwrapped derivative gives the level the surface actually
// deserves.

struct Globals {
    view_projection: mat4x4<f32>,
    // Tiles per row in the atlas, and the pixel geometry of one tile. Passed in
    // rather than baked in so the atlas can be resized without a shader edit.
    atlas_grid: u32,
    atlas_side: u32,
    tile: u32,
    padding: u32,
    // 0 textured, 1 flat. Wireframe is a pipeline state, not a branch.
    render_mode: u32,
    // Task 10's lighting mode: 0 simple, 1 classic. Orthogonal to render_mode,
    // which says what surface data to draw rather than how to light it.
    lighting_mode: u32,
    // Time of day scales the stored sunlight here rather than in the world, so
    // dusk dirties nothing. See `Globals` on the Rust side.
    sun_intensity: f32,
    ambient: f32,
    // Where fog starts, in blocks. It becomes total at `sky_colour.w`.
    fog_start: f32,
    // The three words of padding the Rust side spells out are implicit here:
    // WGSL aligns a vec4 to 16 bytes, so this lands at offset 112 either way.
    sun_colour: vec4<f32>,
    // Sky colour in xyz, fog's far distance in w.
    sky_colour: vec4<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var atlas: texture_2d<f32>;
@group(0) @binding(2) var atlas_sampler: sampler;

struct VertexIn {
    // x:6 | y:6 | z:6 | axis:2 | positive:1 | occlusion:2
    @location(0) packed: u32,
    // material:16 | light:16, the light half a packed `core::light::Light`
    @location(1) material: u32,
    // Per-instance: this chunk's camera-relative offset, in blocks.
    @location(2) chunk_offset: vec4<f32>,
};

struct VertexOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) tile_uv: vec2<f32>,
    @location(1) @interpolate(flat) slot: u32,
    @location(2) shade: f32,
    // Sunlight and block colour, interpolated across the face. **This is what
    // makes lighting smooth**: each vertex carries the average of the four
    // blocks at its corner and the hardware fills in between them, so a
    // surface gets a gradient rather than a per-block staircase.
    @location(3) sun: f32,
    @location(4) block_light: vec3<f32>,
    // Distance from the camera, in blocks. The chunk offset is already
    // camera-relative (floating origin), so this needs no camera position.
    @location(5) distance: f32,
    // Ambient occlusion, 0 darkest to 1 open, interpolated across the face.
    @location(6) occlusion: f32,
};

// Lighting mode 1: directional face shading, matching `mesher::face_shade`.
// Flat-lit voxels are unreadable — every edge disappears and the world is one
// white mass. Task 10 replaces the light byte with propagated light; this
// stays.
// How much light the most occluded corner keeps.
//
// Strong enough to read as shading rather than as a smudge — the first attempt
// kept 55% and was reported from the window as barely visible. Not zero: a
// corner boxed in on both sides is in shadow, not in a different room.
const AO_FLOOR: f32 = 0.35;

fn face_shade(axis: u32, positive: bool) -> f32 {
    if (axis == 1u) {
        if (positive) { return 1.0; }   // top
        return 0.5;                     // bottom
    }
    if (axis == 2u) { return 0.85; }    // z sides
    return 0.75;                        // x sides
}

@vertex
fn vertex_main(input: VertexIn) -> VertexOut {
    let x = f32(input.packed & 0x3Fu);
    let y = f32((input.packed >> 6u) & 0x3Fu);
    let z = f32((input.packed >> 12u) & 0x3Fu);
    let axis = (input.packed >> 18u) & 0x3u;
    let positive = ((input.packed >> 20u) & 1u) == 1u;

    // Sub-node cells to blocks. The mesher emits positions in 0..=48 because
    // that is what fits six bits; a chunk is sixteen blocks.
    let subnodes = 3.0;
    let local = vec3<f32>(x, y, z) / subnodes;

    var out: VertexOut;
    let camera_relative = local + input.chunk_offset.xyz;
    out.clip = globals.view_projection * vec4<f32>(camera_relative, 1.0);
    out.distance = length(camera_relative);

    // The two coordinates that span this face's plane. Must match
    // `SubNodeGrid::cell`: axis 0 spans (y, z), axis 1 spans (x, z), axis 2
    // spans (x, y). A mismatch here rotates the texture on two faces out of
    // six, which reads as a texture bug rather than a mapping one.
    if (axis == 0u) {
        out.tile_uv = vec2<f32>(local.y, local.z);
    } else if (axis == 1u) {
        out.tile_uv = vec2<f32>(local.x, local.z);
    } else {
        out.tile_uv = vec2<f32>(local.x, local.y);
    }

    out.slot = input.material & 0xFFFFu;

    // Unpack `core::light::Light`: sun in the top nibble, then r, g, b.
    // Four bits each, so 15 is full and the divisor is 15 rather than 16 —
    // dividing by 16 leaves a fully lit surface at 94% and the whole world
    // very slightly grey.
    let packed_light = (input.material >> 16u) & 0xFFFFu;
    let levels = vec4<f32>(
        f32((packed_light >> 12u) & 0xFu),
        f32((packed_light >> 8u) & 0xFu),
        f32((packed_light >> 4u) & 0xFu),
        f32(packed_light & 0xFu),
    ) / 15.0;

    out.shade = face_shade(axis, positive);

    // **Occlusion is applied to the colour, not to the light.** Scaling the
    // stored light would keep its hue, so a corner shadowed under a low sun
    // came out dim orange rather than dark. Geometry darkens whatever lands on
    // it. `AO_FLOOR` keeps a boxed-in corner from going black, which reads as a
    // hole in the geometry rather than as shading.
    let level = f32((input.packed >> 21u) & 0x3u) / 3.0;
    out.occlusion = AO_FLOOR + (1.0 - AO_FLOOR) * level;
    out.sun = levels.x;
    out.block_light = levels.yzw;
    return out;
}

// How much of the sky has taken over at this distance, 0 to 1.
//
// Linear between the two distances rather than exponential. Exponential fog is
// prettier in the middle distance and never quite reaches the sky colour, which
// leaves a faint edge exactly where the loaded world stops — the one place this
// fog exists to hide.
fn fog_amount(distance: f32) -> f32 {
    let far = globals.sky_colour.w;
    return clamp((distance - globals.fog_start) / max(far - globals.fog_start, 0.001), 0.0, 1.0);
}

// What one fragment's light comes to, as a colour multiplier.
//
// Sunlight is scaled by the time of day and block light is not: that is the
// whole reason they are separate channels. A cave stays dark at noon and lit by
// its own lamps at midnight, from one stored value per block.
//
// The two are combined with `max` per channel rather than added. Adding them
// blows out to white wherever a lamp stands in daylight, which is most lamps
// anyone places outdoors.
fn lighting(input: VertexOut) -> vec3<f32> {
    // Mode 1: face shading and occlusion only, which is Task 08's world. The
    // vertices still carry light — the mesher was handed a flat daylight value
    // to bake, so they carry the same one everywhere and the branch is what
    // makes that visible rather than merely uniform.
    if (globals.lighting_mode == 0u) {
        return vec3<f32>(input.shade * input.occlusion);
    }
    let daylight = input.sun * globals.sun_intensity * globals.sun_colour.rgb;
    let lit = max(daylight, input.block_light);
    // A floor under the darkest cave, so a dark room is legible rather than
    // pitch black. Presentation, not simulation — the stored light really is
    // zero down there.
    return max(lit, vec3<f32>(globals.ambient)) * input.shade * input.occlusion;
}

@fragment
fn fragment_main(input: VertexOut) -> @location(0) vec4<f32> {
    if (globals.render_mode == 1u) {
        // Flat: directional shading and occlusion, no propagated light.
        // **Mode 1 must keep Task 08's cost profile exactly**, and that
        // includes ignoring light rather than sampling it and discarding it —
        // but occlusion is geometry, and a flat mode without it is unreadable
        // in exactly the way flat lighting is.
        return vec4<f32>(vec3<f32>(input.shade * input.occlusion), 1.0);
    }

    let side = f32(globals.atlas_side);
    let column = f32(input.slot % globals.atlas_grid);
    let row = f32(input.slot / globals.atlas_grid);
    let pitch = f32(globals.tile + globals.padding * 2u);

    let origin = (vec2<f32>(column, row) * pitch + f32(globals.padding)) / side;
    let extent = f32(globals.tile) / side;

    // The derivative is taken from the UNWRAPPED coordinate — see the header.
    let ddx = dpdx(input.tile_uv) * extent;
    let ddy = dpdy(input.tile_uv) * extent;
    let uv = origin + fract(input.tile_uv) * extent;

    let texel = textureSampleGrad(atlas, atlas_sampler, uv, ddx, ddy);
    let lit = texel.rgb * lighting(input);

    // Fog last, over the lit colour rather than under it: fog is between the
    // eye and the surface, so it is not something the surface's own light
    // shines through. Mixing before lighting would let a lamp brighten the air.
    let haze = fog_amount(input.distance);
    return vec4<f32>(mix(lit, globals.sky_colour.rgb, haze), texel.a);
}
