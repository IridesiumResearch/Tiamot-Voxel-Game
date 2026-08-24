// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

// The first-person viewmodel: a hand, and whatever it is holding.
//
// # Why this is in VIEW space and not in the world
//
// A hand is not at a place in the world — it is at a place on the screen, and
// it stays there while the world turns under it. So the geometry here is built
// directly in the camera's own space (x right, y up, z back) and projected with
// the projection matrix ALONE. There is no view matrix in this shader, which is
// the whole trick: nothing to update as the player looks around, and no chance
// of the hand drifting a frame behind the camera it is attached to.
//
// # And why it has no depth test
//
// A hand held forty centimetres from the eye sits inside almost everything: the
// wall you are facing, the block you are digging, your own feet. Depth-testing
// it against the world would have it disappear whenever you stood near
// anything, which is why every game that draws one gives it a depth range of
// its own. Drawn last and unconditionally is the same answer with less
// machinery, and it is correct because nothing is ever meant to be in front.

struct View {
    projection: mat4x4<f32>,
};

@group(0) @binding(0) var<uniform> view: View;
@group(0) @binding(1) var atlas: texture_2d<f32>;
@group(0) @binding(2) var atlas_sampler: sampler;

struct Piece {
    // Centre in view space, in blocks. `w` is a roll about the view's own
    // forward axis, in radians — what a swing rotates.
    @location(0) placement: vec4<f32>,
    // Half-extents in blocks. `w` unused.
    @location(1) size: vec4<f32>,
    // The atlas rectangle to sample: u0, v0, u1, v1. All zero means untextured,
    // which is how an arm is told apart from a block without a second pipeline.
    @location(2) uv: vec4<f32>,
    // Multiplied into whatever comes out. Alpha included, so a piece can fade.
    @location(3) tint: vec4<f32>,
};

struct VertexOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) tint: vec4<f32>,
    @location(2) @interpolate(flat) shade: f32,
    @location(3) @interpolate(flat) textured: f32,
};

// One unit cube: six faces, two triangles each, wound counter-clockwise seen
// from outside. Written out rather than derived from the index with bit tricks,
// for the same reason the blob's quad is: a clever version of this is where a
// face ends up inside out.
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

// How light each face is. Three levels, the same idea as the shape editor's:
// what matters is that a cube reads as a cube, not where the sun is — there is
// no sun in a viewmodel, and a hand that darkened at dusk would look broken.
fn face_shade(index: u32) -> f32 {
    let face = index / 6u;
    if (face == 4u) { return 1.0; }       // top
    if (face == 5u) { return 0.55; }      // bottom
    if (face == 0u || face == 1u) { return 0.85; }
    return 0.7;
}

// Where on the face this corner is, 0..1, for sampling a tile.
fn face_uv(index: u32, position: vec3<f32>) -> vec2<f32> {
    let face = index / 6u;
    if (face == 0u || face == 1u) { return position.xy * 0.5 + 0.5; }
    if (face == 2u || face == 3u) { return position.zy * 0.5 + 0.5; }
    return position.xz * 0.5 + 0.5;
}

@vertex
fn vertex_main(@builtin(vertex_index) index: u32, piece: Piece) -> VertexOut {
    let local = corner(index) * piece.size.xyz;

    // Rolled about the view's forward axis. A swing is a hand rotating in the
    // plane of the screen, which is this and nothing else — a full rotation
    // matrix would be three angles to tune and two of them would be zero.
    let angle = piece.placement.w;
    let c = cos(angle);
    let s = sin(angle);
    let rolled = vec3<f32>(local.x * c - local.y * s, local.x * s + local.y * c, local.z);

    var out: VertexOut;
    out.clip = view.projection * vec4<f32>(rolled + piece.placement.xyz, 1.0);
    out.tint = piece.tint;
    out.shade = face_shade(index);
    let textured = piece.uv.z - piece.uv.x;
    out.textured = select(0.0, 1.0, textured > 0.0);
    let on_face = face_uv(index, corner(index));
    out.uv = mix(piece.uv.xy, piece.uv.zw, on_face);
    return out;
}

@fragment
fn fragment_main(input: VertexOut) -> @location(0) vec4<f32> {
    var colour = input.tint;
    if (input.textured > 0.5) {
        colour = colour * textureSample(atlas, atlas_sampler, input.uv);
    }
    return vec4<f32>(colour.rgb * input.shade, colour.a);
}
