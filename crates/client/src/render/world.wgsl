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
    // World-to-light clip, one per shadow cascade. Only mode 3 reads these.
    light_view_projection: array<mat4x4<f32>, 3>,
    // Where each cascade ends, in blocks, in xyz; one shadow texel in UV in w.
    cascade_far: vec4<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var atlas: texture_2d<f32>;
@group(0) @binding(2) var atlas_sampler: sampler;

// Mode 3 only, and in its own bind group for exactly that reason: a binding
// group is part of a pipeline's layout, so putting these in group 0 would make
// every mode allocate shadow maps to have something to bind. `fragment_main`
// does not mention them and `fragment_shadowed` does, which is what lets the
// two pipelines have different layouts out of one shader file.
@group(1) @binding(0) var shadow_map: texture_depth_2d_array;
@group(1) @binding(1) var shadow_sampler: sampler_comparison;

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
    // Camera-relative position, which is where the shadow lookup happens. The
    // renderer has no world-space coordinate to offer (floating origin), and
    // the light matrices are built in the same space for that reason.
    @location(7) world: vec3<f32>,
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
    out.world = camera_relative;

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
// How far past white a surface at full block light is pushed in mode 3.
//
// **This is what gives bloom something to find.** Every other quantity in this
// shader lands in 0..1, so a threshold at white would never fire and the glow
// pass would be an expensive way to add zero. Pushing block light — and only
// block light — above white means a wall beside a lamp blooms and a wall in
// daylight does not, which is the distinction the effect is supposed to draw.
//
// Stylised rather than physical, as Task 10 asks: a real lamp is not a third
// again as bright as the sun. Charter rule 4 does not reach here; nothing about
// this feeds the simulation.
//
// Was 1.6, which was reported from the window as about 40% too much glow — the
// number is how far past white a lamp-lit surface goes, so it is the knob that
// decides how much of a wall blooms rather than how bright the bloom is.
const EMISSIVE_GAIN: f32 = 1.2;

// How dark the neutral is that occlusion mixes toward, as a fraction of the
// surface's own brightness. Half: dark enough to read as a corner, light enough
// that the geometry in it is still visible.
const AO_NEUTRAL: f32 = 0.5;

// `normalize` makes a unit vector, whose components average about 0.58 rather
// than 1, so the tint needs scaling back up or every occluded corner is darker
// than it was meant to be. Written as a constant rather than folded in because
// it is arithmetic bookkeeping and not a look.
const SKY_TINT_SCALE: f32 = 1.732;

// Perceived brightness, for the grey occlusion mixes toward. The green weight
// dominates because the eye's does.
fn luma(colour: vec3<f32>) -> f32 {
    return dot(colour, vec3<f32>(0.2126, 0.7152, 0.0722));
}

fn lighting(input: VertexOut, shadow: f32) -> vec3<f32> {
    // Mode 1: face shading and occlusion only, which is Task 08's world. The
    // vertices still carry light — the mesher was handed a flat daylight value
    // to bake, so they carry the same one everywhere and the branch is what
    // makes that visible rather than merely uniform.
    if (globals.lighting_mode == 0u) {
        return vec3<f32>(input.shade * input.occlusion);
    }
    // **The shadow scales the SUN and nothing else.** A cave is dark because
    // its stored sunlight channel is zero, which the server worked out and is
    // true whether or not anything is drawing; the shadow map only says whether
    // the sun can see a surface it does reach. Applying it to the total would
    // let a shadow put out a lamp.
    let daylight = input.sun * globals.sun_intensity * globals.sun_colour.rgb * shadow;

    // **Block light loses its hue as it dims, on purpose.**
    //
    // Each channel is stored with its own falloff, one level a block, so the
    // weakest channel reaches zero first and the hue slides toward the
    // strongest as the light gets further from its source: a warm lamp goes
    // orange, then red, then black. Reported from the window as "very yellow
    // near the middle and very red at the edges", which is exactly that.
    //
    // Fading the colour out as the level falls puts the falloff back where it
    // belongs — in the brightness — and turns a saturated red fringe into a
    // dim one. The lamp keeps its colour where it is bright enough for the
    // colour to be what anyone is looking at.
    var block = input.block_light;
    let peak = max(block.r, max(block.g, block.b));
    block = mix(vec3<f32>(peak), block, peak);
    if (globals.lighting_mode == 2u) {
        block = block * EMISSIVE_GAIN;
    }
    let lit = max(daylight, block);
    // A floor under the darkest cave, so a dark room is legible rather than
    // pitch black. Presentation, not simulation — the stored light really is
    // zero down there.
    // The floor under the darkest place is the SKY's colour, not grey.
    //
    // Whatever light reaches a shadowed surface with no lamp on it came from
    // the sky, so it arrives the colour the sky is — bluish by day, and the
    // reason a neutral grey floor read as yellow against a blue world. Scaled
    // right down: this is a floor, not a second light source.
    let ambient = globals.sky_colour.rgb * globals.ambient;
    let shaded = max(lit, ambient) * input.shade;

    // **Occlusion darkens toward grey, not by scaling.**
    //
    // Multiplying was the obvious thing and it cannot work, for a reason worth
    // writing down: multiplication is associative, so `texel * (light * ao)`
    // and `(texel * light) * ao` are the same arithmetic — "apply occlusion to
    // the colour rather than to the light" is a distinction with no difference,
    // and an earlier attempt at this was exactly that. Scaling a warm colour
    // keeps its hue precisely, so a corner beside a lamp came out dim orange
    // and got reported, twice, as the ambient occlusion looking yellow.
    //
    // A real shadow takes the light away, and colour with it. Mixing toward a
    // neutral grey of the same brightness removes the hue as it removes the
    // light, which is what an eye expects a corner to do.
    // Toward the sky's colour at the same brightness, for the reason the
    // ambient floor is: a corner is dark because the sky cannot see into it,
    // and what little reaches it is skylight. A neutral grey was the first
    // attempt and still read as yellow against a blue world.
    let sky_tint = normalize(globals.sky_colour.rgb + vec3<f32>(0.0001));
    let neutral = sky_tint * luma(shaded) * AO_NEUTRAL * SKY_TINT_SCALE;
    return mix(neutral, shaded, input.occlusion);
}

// How much of the sun reaches this fragment, 0 fully shadowed to 1 open.
//
// # Picking a cascade
//
// By distance from the camera, which is what the cascades were split on. The
// first one it fits into wins; past the last, everything is lit. That last case
// is not a fallback — the shadow range stops well short of the far plane on
// purpose, and beyond it distance fog is dissolving the geometry anyway.
//
// # PCF
//
// Nine comparison samples in a 3x3 around the point. `textureSampleCompare`
// does the depth test in hardware and filters the RESULT, so each of these is
// already a bilinear blend of four texels — nine taps behave like a 6x6 kernel.
// A single tap gives a hard, stair-stepped edge along the shadow map's grid,
// which on a voxel world reads as geometry that is not there.
fn shadow_factor(input: VertexOut) -> f32 {
    var cascade = 0;
    if (input.distance > globals.cascade_far.z) {
        return 1.0;
    } else if (input.distance > globals.cascade_far.y) {
        cascade = 2;
    } else if (input.distance > globals.cascade_far.x) {
        cascade = 1;
    }

    let clip = globals.light_view_projection[cascade] * vec4<f32>(input.world, 1.0);
    // No perspective divide worth the name — the light's projection is
    // orthographic, so w is 1 — but doing it costs nothing and means a future
    // spotlight does not need this line rewritten.
    let ndc = clip.xyz / clip.w;
    let uv = vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5);

    // Outside the cascade, or behind the light's near plane: lit. A fragment
    // that falls outside every cascade has no shadow information, and guessing
    // "shadowed" would put a dark band around the edge of the map.
    if (uv.x < 0.0 || uv.x > 1.0 || uv.y < 0.0 || uv.y > 1.0 || ndc.z > 1.0) {
        return 1.0;
    }

    let texel = globals.cascade_far.w;
    var sum = 0.0;
    for (var y = -1; y <= 1; y = y + 1) {
        for (var x = -1; x <= 1; x = x + 1) {
            let offset = vec2<f32>(f32(x), f32(y)) * texel;
            sum = sum + textureSampleCompareLevel(
                shadow_map,
                shadow_sampler,
                uv + offset,
                cascade,
                ndc.z
            );
        }
    }
    return sum / 9.0;
}

// One fragment, given how much sun reaches it.
//
// Shared by both entry points so the atlas lookup, the lighting and the fog
// exist once. The only difference between a shadowed frame and an unshadowed
// one is the number that arrives here.
fn surface(input: VertexOut, shadow: f32) -> vec4<f32> {
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
    let lit = texel.rgb * lighting(input, shadow);

    // Mode 3 fogs in the post chain instead, from the depth buffer — which is
    // what lets its fog reach the sky and take the sun's colour with it. Doing
    // it here as well would apply it twice, and the second application is over
    // a colour that has already lost its contrast to the first.
    if (globals.lighting_mode == 2u) {
        return vec4<f32>(lit, texel.a);
    }

    // Fog last, over the lit colour rather than under it: fog is between the
    // eye and the surface, so it is not something the surface's own light
    // shines through. Mixing before lighting would let a lamp brighten the air.
    let haze = fog_amount(input.distance);
    return vec4<f32>(mix(lit, globals.sky_colour.rgb, haze), texel.a);
}

// Modes 1 and 2: no shadow map exists, so nothing is in shadow.
@fragment
fn fragment_main(input: VertexOut) -> @location(0) vec4<f32> {
    return surface(input, 1.0);
}

// Mode 3: the same surface, with the cascades consulted.
@fragment
fn fragment_shadowed(input: VertexOut) -> @location(0) vec4<f32> {
    return surface(input, shadow_factor(input));
}
