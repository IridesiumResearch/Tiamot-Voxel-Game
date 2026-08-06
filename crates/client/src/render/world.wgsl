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
    // Time of day scales the stored sunlight here rather than in the world, so
    // dusk dirties nothing. See `Globals` on the Rust side.
    sun_intensity: f32,
    ambient: f32,
    pad0: u32,
    sun_colour: vec4<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var atlas: texture_2d<f32>;
@group(0) @binding(2) var atlas_sampler: sampler;

struct VertexIn {
    // x:6 | y:6 | z:6 | axis:2 | positive:1
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
};

// Lighting mode 1: directional face shading, matching `mesher::face_shade`.
// Flat-lit voxels are unreadable — every edge disappears and the world is one
// white mass. Task 10 replaces the light byte with propagated light; this
// stays.
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
    out.clip = globals.view_projection * vec4<f32>(local + input.chunk_offset.xyz, 1.0);

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
    out.sun = levels.x;
    out.block_light = levels.yzw;
    return out;
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
    let daylight = input.sun * globals.sun_intensity * globals.sun_colour.rgb;
    let lit = max(daylight, input.block_light);
    // A floor under the darkest cave, so a dark room is legible rather than
    // pitch black. Presentation, not simulation — the stored light really is
    // zero down there.
    return max(lit, vec3<f32>(globals.ambient)) * input.shade;
}

@fragment
fn fragment_main(input: VertexOut) -> @location(0) vec4<f32> {
    if (globals.render_mode == 1u) {
        // Flat: directional shading only, no propagated light. **Mode 1 must
        // keep Task 08's cost profile exactly**, and that includes ignoring
        // light rather than sampling it and throwing the result away.
        return vec4<f32>(vec3<f32>(input.shade), 1.0);
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
    return vec4<f32>(texel.rgb * input.shade, texel.a);
}
