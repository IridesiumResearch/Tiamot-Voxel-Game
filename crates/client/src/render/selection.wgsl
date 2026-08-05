// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

// The selection outline: line segments in camera-relative cell space.
//
// Its own shader rather than a branch in the world one because it draws a
// different primitive from different data — line segments with no texture, no
// atlas, and no instance buffer. Sharing the world pipeline would mean carrying
// a mode branch through a shader that runs for every vertex of every chunk, to
// serve a dozen lines.
//
// Positions arrive already relative to the camera, in **cells**, so the
// floating origin is applied on the CPU exactly as it is for chunk instances —
// nothing here ever sees a world coordinate (charter rule 7).

struct Globals {
    view_projection: mat4x4<f32>,
    atlas_grid: u32,
    atlas_side: u32,
    tile: u32,
    padding: u32,
    render_mode: u32,
    pad0: u32,
    pad1: u32,
    pad2: u32,
};

@group(0) @binding(0) var<uniform> globals: Globals;

struct VertexOut {
    @builtin(position) clip: vec4<f32>,
};

@vertex
fn vertex_main(@location(0) position: vec3<f32>) -> VertexOut {
    var out: VertexOut;
    // Cells to blocks: the world pipeline works in blocks, and the view
    // projection is built for that space.
    out.clip = globals.view_projection * vec4<f32>(position / 3.0, 1.0);
    return out;
}

@fragment
fn fragment_main() -> @location(0) vec4<f32> {
    // Near-black rather than pure black, and opaque. A selection outline has to
    // read against both a white block and a blue sky, and the one colour that
    // does that everywhere is a dark line — the same choice every voxel game
    // converges on.
    return vec4<f32>(0.05, 0.05, 0.05, 1.0);
}
