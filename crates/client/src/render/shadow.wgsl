// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

// The shadow pass: the world drawn from the sun's point of view, depth only.
//
// # Why this is not the world shader with a different matrix
//
// It could be, and it should not be. A depth-only pass has no fragment stage at
// all, so everything the world shader computes — the atlas lookup, the light
// unpacking, the ambient occlusion, the fog — would be computed and thrown
// away, three times over for three cascades. This unpacks the position and
// stops.
//
// The vertex format is the same eight bytes, deliberately: the shadow pass
// draws the SAME buffers the world pass does, so a mesh that is uploaded once
// is drawn by both.

struct Cascade {
    // World-to-light clip, for the cascade being drawn.
    view_projection: mat4x4<f32>,
};

@group(0) @binding(0) var<uniform> cascade: Cascade;

struct VertexIn {
    // x:6 | y:6 | z:6 | axis:2 | positive:1 | occlusion:2
    @location(0) packed: u32,
    // material:16 | light:16. Unused here, and still declared: the vertex
    // layout is shared with the world pass and a layout that dropped an
    // attribute would need its own buffer.
    @location(1) material: u32,
    // Per-instance: this chunk's camera-relative offset, in blocks.
    @location(2) chunk_offset: vec4<f32>,
};

@vertex
fn vertex_main(input: VertexIn) -> @builtin(position) vec4<f32> {
    // Sub-node coordinates, thirds of a block, exactly as the world shader
    // unpacks them. Any disagreement here shows up as shadows offset from the
    // geometry casting them.
    let x = f32(input.packed & 0x3Fu) / 3.0;
    let y = f32((input.packed >> 6u) & 0x3Fu) / 3.0;
    let z = f32((input.packed >> 12u) & 0x3Fu) / 3.0;
    let position = vec3<f32>(x, y, z) + input.chunk_offset.xyz;
    return cascade.view_projection * vec4<f32>(position, 1.0);
}
