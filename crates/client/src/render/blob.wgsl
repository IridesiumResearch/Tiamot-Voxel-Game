// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

// The blob shadow: a soft dark disc on the ground under a body.
//
// # Why an engine has one of these even with real shadows
//
// Cascaded shadow maps answer one question — is the SUN blocked — and only in
// lighting mode 3. Everywhere else, and for every light that is not the sun, a
// body has nothing anchoring it to the ground: it reads as hovering, and it
// reads that way most strongly indoors and at night, which is exactly where a
// player is looking hardest at where their feet are.
//
// So this is not an approximation of a sun shadow and does not try to be. It is
// a grounding cue, it works in all three lighting modes, it costs one quad per
// body, and it is what Minecraft has been doing since before shadow maps were
// affordable.
//
// # No geometry buffer
//
// The quad is built in the vertex stage from `vertex_index`, so a body costs
// one instance and no vertices. The disc is cut out in the fragment stage by
// distance from the middle, which is also what makes its edge soft — a texture
// would need a sampler, a binding and an asset to say the same thing.

struct Globals {
    view_projection: mat4x4<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;

struct Instance {
    // Where the disc sits, camera-relative, in blocks. Already lifted clear of
    // the surface it lies on by the caller.
    @location(0) centre: vec4<f32>,
    // Radius in blocks in x, opacity in y. Two spare.
    @location(1) shape: vec4<f32>,
};

struct VertexOut {
    @builtin(position) clip: vec4<f32>,
    // Position within the disc, -1 to 1 on each axis.
    @location(0) local: vec2<f32>,
    @location(1) @interpolate(flat) opacity: f32,
};

@vertex
fn vertex_main(@builtin(vertex_index) index: u32, instance: Instance) -> VertexOut {
    // Two triangles, as a strip of six. Written out rather than computed with
    // bit tricks: six pairs is legible and a clever version of the same thing
    // is where a quad ends up wound the wrong way.
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(1.0, -1.0),
        vec2<f32>(-1.0, 1.0),
        vec2<f32>(1.0, -1.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(-1.0, 1.0),
    );
    let local = corners[index];
    let radius = instance.shape.x;

    // Flat on the ground: the disc lies in the xz plane, so its own y never
    // changes. A billboard would face the camera and read as a sticker.
    let world = instance.centre.xyz + vec3<f32>(local.x * radius, 0.0, local.y * radius);

    var out: VertexOut;
    out.clip = globals.view_projection * vec4<f32>(world, 1.0);
    out.local = local;
    out.opacity = instance.shape.y;
    return out;
}

@fragment
fn fragment_main(input: VertexOut) -> @location(0) vec4<f32> {
    // Round, and soft at the rim. `1.0` at the middle falling to nothing at the
    // edge — squared rather than linear, because a linear falloff has a visible
    // ring where it meets zero and this does not.
    let distance = length(input.local);
    let falloff = 1.0 - smoothstep(0.35, 1.0, distance);
    let alpha = input.opacity * falloff * falloff;
    if (alpha <= 0.001) {
        discard;
    }
    // Black, and the alpha does the darkening. Premultiplied is not used here:
    // the pipeline blends `src.a` against `1 - src.a`, so a black source with
    // alpha `a` multiplies what is behind it by `1 - a`.
    return vec4<f32>(0.0, 0.0, 0.0, alpha);
}
