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
// a grounding cue, it works in all three lighting modes, and it is what
// Minecraft has been doing since before shadow maps were affordable.
//
// # A disc is a GRID of quads, not one
//
// One quad lies at one height, and the ground under a body on sub-node terrain
// is not at one height — reported from the window as the shadow floating on
// chiselled ground "instead of projecting down into" it. Each tile sits on the
// ground under its own column.
//
// The falloff is still the DISC's: a tile carries where it sits within the disc
// and how much of it it spans, so the soft rim is round across the group rather
// than each tile fading into its own edge and leaving a grid of dots.
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
    // Where this TILE sits, camera-relative, in blocks. Already lifted clear of
    // the surface it lies on by the caller. `w` is the tile's half-width as a
    // fraction of the disc's radius.
    @location(0) centre: vec4<f32>,
    // Tile half-width in blocks in x, opacity in y, and the tile's offset from
    // the disc's centre in zw — in units of the disc radius.
    @location(1) shape: vec4<f32>,
};

struct VertexOut {
    @builtin(position) clip: vec4<f32>,
    // Position within the DISC, -1 to 1 on each axis — not within the tile.
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
    let corner = corners[index];
    let half = instance.shape.x;

    // Flat on the ground: each tile lies in the xz plane at its own height, so
    // the disc as a whole follows the surface. A billboard would face the
    // camera and read as a sticker.
    let world = instance.centre.xyz + vec3<f32>(corner.x * half, 0.0, corner.y * half);

    var out: VertexOut;
    out.clip = globals.view_projection * vec4<f32>(world, 1.0);
    // Where this corner is in the DISC: the tile's own offset, plus how far
    // across the tile we are scaled into the disc's units.
    out.local = instance.shape.zw + corner * instance.centre.w;
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
