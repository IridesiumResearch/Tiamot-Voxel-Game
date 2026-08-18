// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

// Skinned figures: the engine's humanoid, and anything a mod ships.
//
// # Why this is not the world shader
//
// Because a skinned vertex is a different thing. The world's vertex is eight
// bytes and snaps to a sub-node CELL — six bits an axis — which is exactly
// right for voxels and useless for a figure: a humanoid is 5.4 cells tall and
// 1.8 wide, so a head would be one cell. This vertex carries float positions, a
// normal, four joint indices and four weights, and the vertex stage moves it by
// the matrices its joints are holding this frame.
//
// The lighting is deliberately simpler than the world's. A voxel face knows
// which way it points and what light was propagated to it; a figure has neither
// — it moves through the world rather than being part of it — so it takes the
// sun, the ambient and the fog, and nothing else. Matte white, which is what an
// untextured `engine:humanoid` is.

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

// The cascade being drawn, for `vertex_shadow` only.
//
// **Group 1**, which is where the colour pipelines keep the shadow MAP — and
// these two never coexist: a pipeline either draws a figure into a cascade or
// draws it into the world. Naga only requires the bindings an entry point
// actually reaches, so the colour pipeline's layout leaves group 1 empty and
// the cascade pipeline leaves group 0 empty.
struct Cascade {
    view_projection: mat4x4<f32>,
};

@group(1) @binding(0) var<uniform> cascade: Cascade;

// The joint matrices of every figure being drawn this frame, end to end. An
// instance says where its own palette starts; nothing here needs to know how
// many there are.
//
// **Group 2, not group 1**, and that is not arbitrary: group 1 is the shadow
// map, which only lighting mode 3 binds. Putting the palette there would make
// every mode allocate a shadow map to draw a mob.
@group(2) @binding(0) var<storage, read> palette: array<mat4x4<f32>>;

struct VertexIn {
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
    // Indices into this instance's slice of the palette.
    @location(3) joints: vec4<u32>,
    @location(4) weights: vec4<f32>,
    // Per-instance: where the figure's feet are, camera-relative, in blocks.
    @location(5) offset: vec4<f32>,
    // Per-instance: heading in radians (x), where this figure's palette starts
    // (y, as a bit-cast u32). Two spare.
    @location(6) placement: vec4<f32>,
};

struct VertexOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) normal: vec3<f32>,
    @location(1) distance: f32,
};

// The vertex, moved by its joints.
//
// Four influences, summed. A weight of zero contributes nothing, so a rigidly
// weighted vertex — which is every vertex of the engine's own box-limbed rig —
// costs the same three wasted multiplies as a smoothly weighted one and needs
// no branch to say so.
fn skin(input: VertexIn, base: u32) -> vec4<f32> {
    let local = vec4<f32>(input.position, 1.0);
    var moved = vec4<f32>(0.0, 0.0, 0.0, 0.0);
    moved += input.weights.x * (palette[base + input.joints.x] * local);
    moved += input.weights.y * (palette[base + input.joints.y] * local);
    moved += input.weights.z * (palette[base + input.joints.z] * local);
    moved += input.weights.w * (palette[base + input.joints.w] * local);
    return moved;
}

// The same, for a direction: no translation, so the w stays at zero.
fn skin_normal(input: VertexIn, base: u32) -> vec3<f32> {
    let local = vec4<f32>(input.normal, 0.0);
    var moved = vec4<f32>(0.0, 0.0, 0.0, 0.0);
    moved += input.weights.x * (palette[base + input.joints.x] * local);
    moved += input.weights.y * (palette[base + input.joints.y] * local);
    moved += input.weights.z * (palette[base + input.joints.z] * local);
    moved += input.weights.w * (palette[base + input.joints.w] * local);
    return moved.xyz;
}

// Model space to camera-relative world space.
//
// Cells to blocks, then a rotation about the vertical, then the instance's
// offset. The rotation is here rather than baked into the palette because a
// heading changes every frame and a palette does not — a figure standing still
// and turning would otherwise rebuild eleven matrices to change one angle.
fn place(local: vec3<f32>, yaw: f32, offset: vec3<f32>) -> vec3<f32> {
    let blocks = local / 3.0;
    let s = sin(yaw);
    let c = cos(yaw);
    let turned = vec3<f32>(
        blocks.x * c + blocks.z * s,
        blocks.y,
        -blocks.x * s + blocks.z * c,
    );
    return turned + offset;
}

@vertex
fn vertex_main(input: VertexIn) -> VertexOut {
    let base = bitcast<u32>(input.placement.y);
    let yaw = input.placement.x;

    let posed = skin(input, base).xyz;
    let world = place(posed, yaw, input.offset.xyz);

    var out: VertexOut;
    out.clip = globals.view_projection * vec4<f32>(world, 1.0);
    // The normal is turned by the same heading. Not skinned-and-then-rotated by
    // a normal matrix: every transform here is a rotation and a translation, so
    // the inverse transpose is the rotation itself.
    let n = skin_normal(input, base);
    let s = sin(yaw);
    let c = cos(yaw);
    out.normal = normalize(vec3<f32>(n.x * c + n.z * s, n.y, -n.x * s + n.z * c));
    out.distance = length(world);
    return out;
}

// Depth only, for a shadow cascade. Same skinning, no fragment stage.
@vertex
fn vertex_shadow(input: VertexIn) -> @builtin(position) vec4<f32> {
    let base = bitcast<u32>(input.placement.y);
    let posed = skin(input, base).xyz;
    let world = place(posed, input.placement.x, input.offset.xyz);
    return cascade.view_projection * vec4<f32>(world, 1.0);
}

// Matte white, lit by the sun and the ambient, then fogged.
//
// The half-lambert wrap is not stylistic: a body lit by a straight dot product
// goes flat black on the shaded side, and a figure with a black half reads as a
// hole rather than as a person. Wrapping keeps the far side dim and legible,
// which is what every character shader in every voxel game does.
@fragment
fn fragment_main(input: VertexOut) -> @location(0) vec4<f32> {
    let normal = normalize(input.normal);
    let facing = dot(normal, -globals.sun_direction.xyz);
    let wrapped = clamp(facing * 0.5 + 0.5, 0.0, 1.0);

    let sun = globals.sun_colour.rgb * globals.sun_intensity * wrapped;
    let sky = globals.sky_colour.rgb * globals.ambient;
    // White, because the rig is untextured and skins are a later phase.
    let albedo = vec3<f32>(0.92, 0.92, 0.94);
    let lit = albedo * (sun + sky);

    let far = globals.sky_colour.w;
    let haze = clamp(
        (input.distance - globals.fog_start) / max(far - globals.fog_start, 0.001),
        0.0,
        1.0,
    );
    return vec4<f32>(mix(lit, globals.sky_colour.rgb, haze), 1.0);
}
