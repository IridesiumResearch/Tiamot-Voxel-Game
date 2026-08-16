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
    // The direction the sun's light TRAVELS, in xyz — so a surface faces the
    // sun when its normal opposes this. Unit length. w unused.
    sun_direction: vec4<f32>,
    // The world size of one shadow texel, in blocks, per cascade in xyz. The
    // normal-offset bias is measured in these: a bias smaller than a texel
    // cannot fix a texel-sized quantisation error. w unused.
    shadow_texel: vec4<f32>,
    // Seconds of animation in x, three spare. Read by the fluid stages only.
    fluid: vec4<f32>,
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
    // x:6 | y:6 | z:6 | axis:2 | positive:1 | occlusion:2 | fine light:8
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
    // Which way this face points. Flat, because a voxel face is one of six
    // directions over its whole area and interpolating between two of them
    // would invent normals no geometry has.
    @location(8) @interpolate(flat) normal: vec3<f32>,
};

// The outward normal of a face, from the two bits that describe it.
//
// The mesher winds every face so that this is the direction back-face culling
// keeps, which is what lets the shadow pass cull backs and record exactly the
// surfaces the light lands on.
fn face_normal(axis: u32, positive: bool) -> vec3<f32> {
    let sign = select(-1.0, 1.0, positive);
    if (axis == 0u) { return vec3<f32>(sign, 0.0, 0.0); }
    if (axis == 1u) { return vec3<f32>(0.0, sign, 0.0); }
    return vec3<f32>(0.0, 0.0, sign);
}

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

// The shared vertex stage, as a plain function.
//
// **A plain function and not the entry point**, because WGSL forbids calling an
// entry point: `fluid_vertex` needs everything this does and then one more
// thing, and the alternative to factoring it out is two copies of the unpacking
// that must never disagree.
fn unpack_vertex(input: VertexIn) -> VertexOut {
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
    let coarse = vec4<f32>(
        f32((packed_light >> 12u) & 0xFu),
        f32((packed_light >> 8u) & 0xFu),
        f32((packed_light >> 4u) & 0xFu),
        f32(packed_light & 0xFu),
    );
    // **And two more bits a channel, from the position word's spare room.**
    //
    // A level is four bits, so light falling one level per block has nothing
    // between one level and the next to say, and a lamp's gradient came out as
    // a hard band at every block boundary however the corners were sampled.
    // Quarter levels are what make it a ramp. See `crate::shade`.
    let fine_bits = (input.packed >> 23u) & 0xFFu;
    let fine = vec4<f32>(
        f32((fine_bits >> 6u) & 0x3u),
        f32((fine_bits >> 4u) & 0x3u),
        f32((fine_bits >> 2u) & 0x3u),
        f32(fine_bits & 0x3u),
    );
    // Four bits is 0..15, so the divisor is 15 rather than 16 — dividing by 16
    // leaves a fully lit surface at 94% and the whole world very slightly grey.
    let levels = (coarse + fine * 0.25) / 15.0;

    out.shade = face_shade(axis, positive);
    out.normal = face_normal(axis, positive);

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

@vertex
fn vertex_main(input: VertexIn) -> VertexOut {
    return unpack_vertex(input);
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

// How much of the original brightness survives the steeper falloff, at the
// source. Squaring a level in 0..1 only ever darkens, so without this a lamp
// standing next to you is dimmer than it was; 1.35 puts its brightest ring back
// where it started.
const FALLOFF: f32 = 1.35;

// How dark the neutral is that occlusion mixes toward, as a fraction of the
// surface's own brightness. Half: dark enough to read as a corner, light enough
// that the geometry in it is still visible.
const AO_NEUTRAL: f32 = 0.5;

// `normalize` makes a unit vector, whose components average about 0.58 rather
// than 1, so the tint needs scaling back up or every occluded corner is darker
// than it was meant to be. Written as a constant rather than folded in because
// it is arithmetic bookkeeping and not a look.
const SKY_TINT_SCALE: f32 = 1.732;

// How much of its daylight a surface keeps when the sun cannot reach it.
//
// **This is the dial for how dark mode 3's shadows are.** A shadow on a sunny
// day is blue, not black: the sun is one light and the sky is another, and only
// the first of them can be stood in front of. Before this existed, a shadowed
// surface fell all the way to the ambient floor — a thirtieth of full — which
// is why the underside of a block had to be left fully lit to be visible at
// all.
//
// It is a FLOOR rather than a share, which matters: an open surface in full sun
// takes the sun's own value unchanged, so modes 1 and 2 — which have no shadow
// map and pass 1.0 — render exactly what they did before. Written as a share
// first, and every lit surface in the world went faintly blue, which the
// screenshot gates caught by noticing that stone had become bluer than red.
const SKY_FLOOR: f32 = 0.3;

// Perceived brightness, for the grey occlusion mixes toward. The green weight
// dominates because the eye's does.
fn luma(colour: vec3<f32>) -> f32 {
    return dot(colour, vec3<f32>(0.2126, 0.7152, 0.0722));
}

// How wide mode 2's terminator is, in `dot(normal, sunward)`.
//
// Wider than mode 3's, and deliberately. Mode 3 has a depth map to say where a
// shadow actually falls, so its terminator only has to cover the few minutes
// either side of a face turning away from the sun. Mode 2 has nothing but the
// facing test, so this band IS its entire soft edge — and a narrow one makes
// every wall of one orientation flip from lit to unlit on a single frame.
const SOFT_TERMINATOR: f32 = 0.35;

// What mode 2 keeps of the ambient floor a mod asked for.
//
// **Underground, and only underground.** Reported from the window: "the ambient
// light in caves should be cut to a third of what it is. It is too bright
// underground." A third, exactly as asked.
//
// It is applied here rather than by lowering `sky.ambient` because that value
// belongs to a mod (charter rule 1) and the other modes are reading it too. What
// varies between modes is how much of the floor each one needs to stay legible,
// which is presentation, which is this shader's business.
const CLASSIC_AMBIENT: f32 = 1.0 / 3.0;

// The least light mode 1 leaves on a surface the sun and every lamp have both
// abandoned. Not zero: a pitch-black cave in the mode whose whole selling point
// is that it is cheap and legible is a cave nobody can find their way out of.
//
// A twelfth. Dark enough that walking underground is obviously a different
// place from walking on the surface, light enough that geometry still reads.
const SIMPLE_FLOOR: f32 = 1.0 / 12.0;

fn lighting(input: VertexOut, shadow: f32) -> vec3<f32> {
    // **Mode 1: one brightness, no hue, no sky.**
    //
    // It used to be `shade * occlusion` and nothing else — the mesher was handed
    // a flat daylight value, so every vertex carried the same light and a cave
    // was exactly as bright as a field at noon. Reported from the window as mode
    // 1 needing to make "beneath the ground dark" and to have "a day night cycle
    // even if it is just an across the board darkening", which is precisely what
    // this is.
    //
    // Mode 1 now meshes with the real propagated light like the others (see
    // `LightingMode::uses_propagated_light`), and spends it on ONE number. The
    // sun channel scaled by the time of day gives the day/night cycle and the
    // dark cave in the same term, because stored sunlight is already zero
    // underground; block light comes in as a monochrome peak so a lamp is still
    // worth carrying down there.
    //
    // What mode 1 deliberately does not do is everything below this branch: no
    // sky hue, no separate sun and skylight, no per-channel falloff, no
    // occlusion neutral. That is what keeps it the cheap mode.
    if (globals.lighting_mode == 0u) {
        let day = input.sun * globals.sun_intensity;
        let lamp = max(input.block_light.r, max(input.block_light.g, input.block_light.b));
        let level = max(max(day, lamp), SIMPLE_FLOOR);
        return vec3<f32>(input.shade * input.occlusion * level);
    }
    // The sky's own colour at unit brightness. Used for the skylight below and
    // for the two floors further down, all of which want the hue of the sky
    // without its brightness — see `AMBIENT_FLOOR` for why that distinction is
    // load-bearing.
    let sky_hue = normalize(globals.sky_colour.rgb + vec3<f32>(0.0001)) * SKY_TINT_SCALE;

    // **Daylight is two things, and only one of them casts a shadow.**
    //
    // The sun is a point and something can stand in front of it; the sky is a
    // dome and nothing can. Splitting them is what stops the underside of a
    // block — which the sun can never see — from coming out either fully lit
    // (it was, before there was a facing test) or black (it would be, if the
    // shadow gated everything). An underside in the open is lit by the sky,
    // which is exactly what it looks like.
    //
    // Both are scaled by the STORED sunlight channel, which the server worked
    // out and which is zero inside a cave. That is charter rule 19's gate: a
    // shadow map may darken what the sun reaches and may never brighten what
    // the sky does not.
    //
    // Combined with `max` rather than added, the same way daylight and block
    // light are below and for the same reason: adding a second light source to
    // every outdoor surface washes the world out. The sky is the floor the sun
    // is measured against, so full sun is exactly full sun.
    let sky_reach = input.sun * globals.sun_intensity;
    let direct = sky_reach * globals.sun_colour.rgb * shadow;
    let skylight = sky_reach * sky_hue * SKY_FLOOR;
    let daylight = max(direct, skylight);

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

    // Steeper than the stored falloff, which is linear because the flood
    // subtracts one level a block. Light does not actually fall off linearly,
    // and a lamp whose glow reaches fifteen blocks at an even rate reads as a
    // flat pool rather than as a source. Squaring the level pulls the far half
    // down and leaves the near half alone, which is where a lamp looks like a
    // lamp.
    let steep = peak * peak * FALLOFF;
    block = block * select(0.0, steep / peak, peak > 0.0);

    // And the hue fades as the level does, but not as fast as it did: the
    // square root keeps a lamp's colour through the middle of its range and
    // only lets go at the edge, where the stored channels have collapsed onto
    // whichever one survived and the colour is an artefact rather than a
    // choice. `mix` by `peak` directly was reported as a little washed out.
    block = mix(vec3<f32>(steep), block, sqrt(peak));
    if (globals.lighting_mode == 2u) {
        block = block * EMISSIVE_GAIN;
    }
    let lit = max(daylight, block);
    // A floor under the darkest cave, so a dark room is legible rather than
    // pitch black. Presentation, not simulation — the stored light really is
    // zero down there.
    //
    // **Its brightness is fixed and only its hue follows the sky.** It used to
    // be `sky_colour * ambient`, and the sky's colour carries the sky's
    // brightness, so the floor rose and fell with the sun — reported from the
    // window as a sealed cave getting brighter as day came, which is the one
    // thing stored sunlight exists to prevent. Taking the direction of the sky
    // colour and none of its length keeps the floor the tint it should be
    // (bluish, which is why a neutral grey read as yellow against a blue world)
    // at a brightness nothing outside can move.
    //
    // Anything that SHOULD brighten with the day is daylight above, gated by
    // the stored sunlight channel, and a sealed cave has none of that.
    var floor_level = globals.ambient;
    if (globals.lighting_mode == 1u) {
        floor_level = floor_level * CLASSIC_AMBIENT;
    }
    let ambient = sky_hue * floor_level;
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
    let neutral = sky_hue * luma(shaded) * AO_NEUTRAL;
    return mix(neutral, shaded, input.occlusion);
}

// How wide the band is, in `dot(normal, sunward)`, over which a face turns from
// facing the sun to facing away.
//
// Zero would be physically right and reads as a switch: every wall of one
// orientation in view flips from lit to unlit on the same frame as the sun
// crosses its plane. A narrow band spreads that over a few minutes of the
// cycle, which is what a terminator looks like.
const TERMINATOR: f32 = 0.12;

// How many shadow texels the sample point is pushed along the surface normal.
//
// **This is what replaced culling front faces in the depth pass.** Recording
// the far side of a caster does move acne off the lit surface, and it does it
// by putting the recorded depth a whole block deep, which shortens every shadow
// by the thickness of the thing casting it — reported from the window as
// shadows falling short of the corner between two blocks and light bleeding in.
// Offsetting along the normal instead fixes the same quantisation at its
// source: the error is that a sample lands in a texel whose depth was taken
// somewhere else on the surface, and moving the sample off the surface by more
// than a texel is worth means it cannot land behind it.
//
// Just over one texel, scaled by how obliquely the light strikes: a face the
// sun grazes spans many texels of depth and needs the most.
const NORMAL_BIAS_TEXELS: f32 = 1.5;

// How much of the sun reaches this fragment, 0 fully shadowed to 1 open.
//
// # Facing comes first, and no depth map can overrule it
//
// A surface the sun is behind is not lit by the sun, whatever the shadow map
// says. This used to be missing entirely, and the symptom was the underside of
// every block coming out fully lit: with front faces culled, the depth recorded
// for an underside was the underside itself, so it compared equal to its own
// depth and passed. Testing the normal answers it without a texture read, and
// removes the acne on grazing faces as a side effect — those are the fragments
// where a depth comparison is least able to be right.
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
    let sunward = -globals.sun_direction.xyz;
    let facing = dot(input.normal, sunward);
    if (facing <= 0.0) {
        return 0.0;
    }
    // The most sun this fragment can receive whatever the map says. Every
    // path out of this function is multiplied by it, including the two that
    // leave before a texture is ever read — a face at the terminator beyond
    // the last cascade is still at the terminator.
    let band = smoothstep(0.0, TERMINATOR, facing);

    var cascade = 0;
    if (input.distance > globals.cascade_far.z) {
        return band;
    } else if (input.distance > globals.cascade_far.y) {
        cascade = 2;
    } else if (input.distance > globals.cascade_far.x) {
        cascade = 1;
    }

    // Along the normal, by more than one texel of the cascade doing the
    // looking, and by more again the more obliquely the sun strikes. `facing`
    // is the cosine, so `1 / facing` is how far a texel's worth of surface
    // travels in depth; clamped because it runs away at the terminator, where
    // the band below is taking over anyway.
    let slope = min(1.0 / max(facing, 0.05), 4.0);
    let bias = globals.shadow_texel[cascade] * NORMAL_BIAS_TEXELS * slope;
    let sample_at = input.world + input.normal * bias;

    let clip = globals.light_view_projection[cascade] * vec4<f32>(sample_at, 1.0);
    // No perspective divide worth the name — the light's projection is
    // orthographic, so w is 1 — but doing it costs nothing and means a future
    // spotlight does not need this line rewritten.
    let ndc = clip.xyz / clip.w;
    let uv = vec2<f32>(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5);

    // Outside the cascade, or behind the light's near plane: lit. A fragment
    // that falls outside every cascade has no shadow information, and guessing
    // "shadowed" would put a dark band around the edge of the map.
    if (uv.x < 0.0 || uv.x > 1.0 || uv.y < 0.0 || uv.y > 1.0 || ndc.z > 1.0) {
        return band;
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
    // The terminator last, over the map's answer rather than instead of it: a
    // face nearly edge-on to the sun is both barely lit and barely able to be
    // sampled correctly, and fading it out covers the second as well as the
    // first.
    return (sum / 9.0) * smoothstep(0.0, TERMINATOR, facing);
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

// Mode 2's shadows, which are not shadows.
//
// **A generic shadow: what a face's orientation says, with nothing asked about
// what is in front of it.** Mode 3 spends three cascades and a PCF kernel
// answering "is anything between this fragment and the sun". Mode 2 answers the
// far cheaper half of the same question — "is this fragment even pointing at the
// sun" — and gets most of the shape for none of the memory.
//
// It is exactly `shadow_factor`'s first two lines with the depth map removed,
// which is the property worth having: the two modes cannot disagree about which
// faces the sun is behind, because there is one rule and mode 3 adds to it
// rather than replacing it. A north wall is dark in both; only mode 3 knows the
// tower next to it is throwing a shadow across the ground.
//
// The result multiplies the SUN term alone, so a face turned away from the sun
// falls to skylight (`SKY_FLOOR`) rather than to black, and a cave stays exactly
// as dark as the stored sunlight says. Charter rule 19's gate again: this may
// darken what the sun reaches and may never brighten what the sky does not.
fn generic_shadow(input: VertexOut) -> f32 {
    let sunward = -globals.sun_direction.xyz;
    return smoothstep(0.0, SOFT_TERMINATOR, dot(input.normal, sunward));
}

// ---------------------------------------------------------------------------
// Fluid
// ---------------------------------------------------------------------------
//
// Milk is drawn in its own blended pass, after every chunk's opaque geometry,
// from a twelve-byte vertex that carries two things terrain has no use for:
// where the surface really sits, and which way it is running.

// Fine units per cell in a fluid vertex's drop. Must match `mesher::FINE`.
const FLUID_FINE: f32 = 16.0;

// Cells per block. Must match `tiamot_core::SUBNODES_PER_AXIS`.
const CELLS_PER_BLOCK: f32 = 3.0;

// How much of what is behind it a fluid surface lets through.
//
// **Reported from the window: "water should be semi transparent when inside and
// out. right now I just see out to the rest of the world and from the outside
// it is opaque."** Both halves of that were one bug — milk was in the opaque
// pass, so it could only ever be a wall, and from inside its own near surface
// was back-face culled and simply was not there at all.
//
// A shade under three quarters: enough to see the shape of the bottom through a
// pond and to know that a river has stones in it, not so much that a deep pool
// stops reading as deep. Engine-wide rather than per fluid, which is the honest
// limit here: `register_fluid` has a `color` and no alpha, and adding one is a
// protocol change rather than a shader constant.
const FLUID_ALPHA: f32 = 0.72;

// Blocks per second a flowing surface's texture travels at full flow.
//
// Slower than the milk itself would move, deliberately. A scroll matched to a
// real flow speed reads as a conveyor belt; what sells moving water is a drift
// slow enough that the eye reads it as the surface being disturbed.
const FLOW_SPEED: f32 = 0.35;

// How far still milk wanders, in blocks, and how fast.
//
// Not a wave — there is no displacement here, only the texture sliding in a
// small circle. It is what stops a settled pond from looking like a painted
// floor, and it is deliberately at the edge of noticeable.
const RIPPLE_SIZE: f32 = 0.02;
const RIPPLE_SPEED: f32 = 0.6;

struct FluidIn {
    @location(0) packed: u32,
    @location(1) material: u32,
    @location(2) chunk_offset: vec4<f32>,
    // drop:16 | flow_x:8 | flow_z:8 — see `mesher::FluidVertex`.
    @location(3) surface: u32,
};

struct FluidOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) tile_uv: vec2<f32>,
    @location(1) @interpolate(flat) slot: u32,
    @location(2) shade: f32,
    @location(3) sun: f32,
    @location(4) block_light: vec3<f32>,
    @location(5) distance: f32,
    @location(6) occlusion: f32,
    @location(7) world: vec3<f32>,
    @location(8) @interpolate(flat) normal: vec3<f32>,
    // Which way this surface runs, in blocks per second, flat across the quad.
    @location(9) @interpolate(flat) flow: vec2<f32>,
};

// A signed byte out of a word. WGSL has no i8, so the sign is put back by hand.
fn signed_byte(word: u32, shift: u32) -> f32 {
    let raw = (word >> shift) & 0xFFu;
    let value = select(f32(raw), f32(raw) - 256.0, raw > 127u);
    return value / 127.0;
}

@vertex
fn fluid_vertex(input: FluidIn) -> FluidOut {
    // The same unpacking as `vertex_main`, and then the one thing that differs.
    var base: VertexIn;
    base.packed = input.packed;
    base.material = input.material;
    base.chunk_offset = input.chunk_offset;
    let lit = unpack_vertex(base);

    var out: FluidOut;
    out.tile_uv = lit.tile_uv;
    out.slot = lit.slot;
    out.shade = lit.shade;
    out.sun = lit.sun;
    out.block_light = lit.block_light;
    out.occlusion = lit.occlusion;
    out.normal = lit.normal;

    // **The drop is what makes the surface smooth.**
    //
    // The occupancy this vertex came from is on the sub-node lattice and can
    // only be a whole number of cells deep. The real surface of a half-full
    // block is not, so the mesher records where it actually is and this lowers
    // the vertex to it — bilinearly across the quad, which is exactly the field
    // `SubNodeGrid::surface_at` describes. Zero for every vertex that is not on
    // the surface, so the sides and the bottom of a pool are untouched.
    //
    // `fill_fluid` rounds the occupancy UP, so this is never negative and a
    // vertex can never escape the geometry its own face culling assumed.
    let drop = f32(input.surface & 0xFFFFu) / FLUID_FINE / CELLS_PER_BLOCK;
    let lowered = lit.world - vec3<f32>(0.0, drop, 0.0);
    out.clip = globals.view_projection * vec4<f32>(lowered, 1.0);
    out.distance = length(lowered);
    out.world = lowered;

    out.flow = vec2<f32>(signed_byte(input.surface, 16u), signed_byte(input.surface, 24u));
    return out;
}

@fragment
fn fluid_fragment(input: FluidOut) -> @location(0) vec4<f32> {
    // Rebuilt rather than shared, because the fluid stage carries one attribute
    // more than the world stage and WGSL has no subtyping to say so.
    var lit: VertexOut;
    lit.clip = input.clip;
    lit.tile_uv = input.tile_uv;
    lit.slot = input.slot;
    lit.shade = input.shade;
    lit.sun = input.sun;
    lit.block_light = input.block_light;
    lit.distance = input.distance;
    lit.occlusion = input.occlusion;
    lit.world = input.world;
    lit.normal = input.normal;

    // **Flowing milk scrolls; still milk ripples.**
    //
    // The direction comes from the surface's own gradient, worked out where the
    // heights were (see `mesher::flow_at`), so a spring on a slope runs downhill
    // and a settled pond has nothing to run. Which means the choice between the
    // two is not a branch on some flag — it falls out of `flow` being zero.
    let time = globals.fluid.x;
    let speed = length(input.flow);
    let drift = input.flow * time * FLOW_SPEED;
    // The ripple fades in exactly as the flow fades out, so a pool feeding a
    // stream has no line across it where one becomes the other.
    let still = clamp(1.0 - speed * 4.0, 0.0, 1.0);
    let wobble = vec2<f32>(
        sin(time * RIPPLE_SPEED),
        cos(time * RIPPLE_SPEED * 0.7)
    ) * RIPPLE_SIZE * still;
    lit.tile_uv = input.tile_uv + drift + wobble;

    // Mode 2's generic shadow applies to milk as much as to anything else; mode
    // 3's cascades do not, because the fluid pass has no shadow bind group.
    var shadow = 1.0;
    if (globals.lighting_mode == 1u) {
        shadow = generic_shadow(lit);
    }
    let colour = surface(lit, shadow);
    return vec4<f32>(colour.rgb, colour.a * FLUID_ALPHA);
}

// Modes 1 and 2. Neither has a shadow map; mode 2 has an opinion anyway.
@fragment
fn fragment_main(input: VertexOut) -> @location(0) vec4<f32> {
    // Mode 1 is flat by construction — it has no sun direction in its lighting
    // at all, only the axis constants in `face_shade` — so handing it a facing
    // term would be shading it twice by two different rules.
    if (globals.lighting_mode == 0u) {
        return surface(input, 1.0);
    }
    return surface(input, generic_shadow(input));
}

// Mode 3: the same surface, with the cascades consulted.
@fragment
fn fragment_shadowed(input: VertexOut) -> @location(0) vec4<f32> {
    return surface(input, shadow_factor(input));
}
