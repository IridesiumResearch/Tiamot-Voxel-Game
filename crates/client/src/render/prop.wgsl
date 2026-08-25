// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

// A prop: a textured box standing somewhere in the world, on somebody's rig.
//
// # What this is for, and why the viewmodel could not do it
//
// A player in third person is looking at their own body, and a body holding
// nothing is a body that has put its tools down. The first-person viewmodel
// draws the same thing, but it draws in VIEW space with no depth test — a hand
// is at a place on the screen, and it is nearer than everything by
// construction. Neither is true here: a held block is at a place in the WORLD,
// it goes behind the wall you walk past, and the figure's arm swings in front
// of it.
//
// # A whole matrix per instance
//
// The blob shadow gets away with a centre and a radius because a disc lying
// flat has no orientation. This does: it hangs off an animated joint, so its
// rotation is whatever the clip has done to that arm this frame, and there is
// no cheaper description of that than the matrix itself. Four vec4s, built on
// the CPU where the joint palette already is.

struct Globals {
    view_projection: mat4x4<f32>,
    atlas_grid: u32,
    atlas_side: u32,
    tile: u32,
    padding: u32,
    render_mode: u32,
    lighting_mode: u32,
    sun_intensity: f32,
    ambient: f32,
    fog_start: f32,
    sun_colour: vec4<f32>,
    sky_colour: vec4<f32>,
    light_view_projection: array<mat4x4<f32>, 3>,
    cascade_far: vec4<f32>,
    sun_direction: vec4<f32>,
    shadow_texel: vec4<f32>,
    fluid: vec4<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var atlas: texture_2d<f32>;
@group(0) @binding(2) var atlas_sampler: sampler;

struct Instance {
    // The model matrix, camera-relative, by column.
    @location(0) column0: vec4<f32>,
    @location(1) column1: vec4<f32>,
    @location(2) column2: vec4<f32>,
    @location(3) column3: vec4<f32>,
    // The atlas rectangle: u0, v0, u1, v1.
    @location(4) uv: vec4<f32>,
};

struct VertexOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) distance: f32,
};

// One unit cube: six faces, two triangles each, wound counter-clockwise seen
// from outside. Written out rather than derived from the index, for the reason
// the viewmodel's copy gives: a clever version of this is where a face ends up
// inside out.
fn corner(index: u32) -> vec3<f32> {
    var cube = array<vec3<f32>, 36>(
        // +z
        vec3<f32>(-1.0, -1.0, 1.0), vec3<f32>(1.0, -1.0, 1.0), vec3<f32>(1.0, 1.0, 1.0),
        vec3<f32>(-1.0, -1.0, 1.0), vec3<f32>(1.0, 1.0, 1.0), vec3<f32>(-1.0, 1.0, 1.0),
        // -z
        vec3<f32>(1.0, -1.0, -1.0), vec3<f32>(-1.0, -1.0, -1.0), vec3<f32>(-1.0, 1.0, -1.0),
        vec3<f32>(1.0, -1.0, -1.0), vec3<f32>(-1.0, 1.0, -1.0), vec3<f32>(1.0, 1.0, -1.0),
        // +x
        vec3<f32>(1.0, -1.0, 1.0), vec3<f32>(1.0, -1.0, -1.0), vec3<f32>(1.0, 1.0, -1.0),
        vec3<f32>(1.0, -1.0, 1.0), vec3<f32>(1.0, 1.0, -1.0), vec3<f32>(1.0, 1.0, 1.0),
        // -x
        vec3<f32>(-1.0, -1.0, -1.0), vec3<f32>(-1.0, -1.0, 1.0), vec3<f32>(-1.0, 1.0, 1.0),
        vec3<f32>(-1.0, -1.0, -1.0), vec3<f32>(-1.0, 1.0, 1.0), vec3<f32>(-1.0, 1.0, -1.0),
        // +y
        vec3<f32>(-1.0, 1.0, 1.0), vec3<f32>(1.0, 1.0, 1.0), vec3<f32>(1.0, 1.0, -1.0),
        vec3<f32>(-1.0, 1.0, 1.0), vec3<f32>(1.0, 1.0, -1.0), vec3<f32>(-1.0, 1.0, -1.0),
        // -y
        vec3<f32>(-1.0, -1.0, -1.0), vec3<f32>(1.0, -1.0, -1.0), vec3<f32>(1.0, -1.0, 1.0),
        vec3<f32>(-1.0, -1.0, -1.0), vec3<f32>(1.0, -1.0, 1.0), vec3<f32>(-1.0, -1.0, 1.0),
    );
    return cube[index];
}

// The outward normal of the face this corner belongs to, in the box's own axes.
fn face_normal(index: u32) -> vec3<f32> {
    let face = index / 6u;
    if (face == 0u) { return vec3<f32>(0.0, 0.0, 1.0); }
    if (face == 1u) { return vec3<f32>(0.0, 0.0, -1.0); }
    if (face == 2u) { return vec3<f32>(1.0, 0.0, 0.0); }
    if (face == 3u) { return vec3<f32>(-1.0, 0.0, 0.0); }
    if (face == 4u) { return vec3<f32>(0.0, 1.0, 0.0); }
    return vec3<f32>(0.0, -1.0, 0.0);
}

// Where on the face this corner is, 0..1, for sampling a tile.
fn face_uv(index: u32, position: vec3<f32>) -> vec2<f32> {
    let face = index / 6u;
    if (face == 0u || face == 1u) { return position.xy * 0.5 + 0.5; }
    if (face == 2u || face == 3u) { return position.zy * 0.5 + 0.5; }
    return position.xz * 0.5 + 0.5;
}

@vertex
fn vertex_main(@builtin(vertex_index) index: u32, instance: Instance) -> VertexOut {
    let model = mat4x4<f32>(
        instance.column0,
        instance.column1,
        instance.column2,
        instance.column3,
    );
    let local = corner(index);
    let world = (model * vec4<f32>(local, 1.0)).xyz;

    var out: VertexOut;
    out.clip = globals.view_projection * vec4<f32>(world, 1.0);
    // A direction, so w is zero and the translation drops out. The matrix is a
    // rotation and a uniform scale, so normalising afterwards is the whole of
    // the inverse transpose — the same argument the skinned shader makes.
    out.normal = normalize((model * vec4<f32>(face_normal(index), 0.0)).xyz);
    out.distance = length(world);
    out.uv = mix(instance.uv.xy, instance.uv.zw, face_uv(index, local));
    return out;
}

// Lit the way a FIGURE is lit, not the way the world is.
//
// A held block is carried by a body and reads as part of it, so it takes the
// same half-lambert wrap the rig does: a straight dot product goes flat black
// on the shaded side, and a black face on a small object reads as a hole. The
// world's own lighting is not available here anyway — a prop is not in a chunk
// and has no propagated light to sample.
@fragment
fn fragment_main(input: VertexOut) -> @location(0) vec4<f32> {
    let normal = normalize(input.normal);
    let facing = dot(normal, -globals.sun_direction.xyz);
    let wrapped = clamp(facing * 0.5 + 0.5, 0.0, 1.0);

    let sun = globals.sun_colour.rgb * globals.sun_intensity * wrapped;
    let sky = globals.sky_colour.rgb * globals.ambient;
    let albedo = textureSample(atlas, atlas_sampler, input.uv);
    let lit = albedo.rgb * (sun + sky);

    let far = globals.sky_colour.w;
    let haze = clamp(
        (input.distance - globals.fog_start) / max(far - globals.fog_start, 0.001),
        0.0,
        1.0,
    );
    return vec4<f32>(mix(lit, globals.sky_colour.rgb, haze), 1.0);
}
