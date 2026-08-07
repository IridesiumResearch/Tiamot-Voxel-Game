// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

// The post chain for lighting mode 3: threshold, separable blur, composite.
//
// One shader with several entry points rather than one file per pass. They
// share a vertex stage and a uniform layout, and splitting them would mean four
// copies of both — which is how two of them end up disagreeing about what
// `texel` means.
//
// # The fullscreen triangle
//
// Three vertices, no vertex buffer, no index buffer. A quad made of two
// triangles has a seam down its diagonal where the two halves meet, and GPUs
// rasterise the shared edge twice; one oversized triangle clipped to the
// viewport has neither problem and one fewer draw's worth of state.

struct Post {
    // One texel of the SOURCE texture, in UV. The blur steps along it, so a
    // blur reading a half-resolution source with a full-resolution texel size
    // samples the same texel nine times and does nothing at all.
    texel: vec2<f32>,
    // Pass-specific: the threshold's cutoff and knee, or the blur's direction.
    params: vec2<f32>,
    // How much bloom to add back in the composite.
    intensity: f32,
    // Exposure applied before the tonemap.
    exposure: f32,
    _pad: vec2<f32>,
};

@group(0) @binding(0) var<uniform> post: Post;
@group(0) @binding(1) var source: texture_2d<f32>;
@group(0) @binding(2) var source_sampler: sampler;
@group(0) @binding(3) var bloom: texture_2d<f32>;

struct VertexOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vertex_main(@builtin(vertex_index) index: u32) -> VertexOut {
    // (-1,-1), (3,-1), (-1,3): a triangle that covers the viewport and is
    // clipped to it.
    let x = f32(i32(index) / 2) * 4.0 - 1.0;
    let y = f32(i32(index) & 1) * 4.0 - 1.0;

    var out: VertexOut;
    out.clip = vec4<f32>(x, y, 0.0, 1.0);
    // Y flips because clip space counts up and texture coordinates count down.
    out.uv = vec2<f32>(x * 0.5 + 0.5, 0.5 - y * 0.5);
    return out;
}

// Perceived brightness. The green weight dominates because the eye's does.
fn luma(colour: vec3<f32>) -> f32 {
    return dot(colour, vec3<f32>(0.2126, 0.7152, 0.0722));
}

// What is bright enough to bloom.
//
// A soft knee rather than a hard cut: a hard threshold makes a surface pop into
// glowing the instant a lamp's light crosses it, and the edge of that pop
// crawls across the wall as the sun moves. The knee spreads the transition over
// a band so brightness fades into glow.
@fragment
fn threshold_main(input: VertexOut) -> @location(0) vec4<f32> {
    let colour = textureSample(source, source_sampler, input.uv).rgb;
    let brightness = luma(colour);
    let cutoff = post.params.x;
    let knee = max(post.params.y, 0.0001);
    let weight = clamp((brightness - cutoff) / knee, 0.0, 1.0);
    return vec4<f32>(colour * weight, 1.0);
}

// One half of a separable Gaussian: nine taps along `params`.
//
// Separable because a 9x9 kernel is 81 samples in one pass and 18 across two,
// for the same result. `params` is (1,0) horizontally and (0,1) vertically, and
// the pass is run twice.
@fragment
fn blur_main(input: VertexOut) -> @location(0) vec4<f32> {
    // Binomial weights, normalised. Symmetric, so only half are listed.
    let weights = array<f32, 5>(0.227027, 0.194594, 0.121621, 0.054054, 0.016216);
    let step = post.texel * post.params;

    var sum = textureSample(source, source_sampler, input.uv).rgb * weights[0];
    for (var i = 1; i < 5; i = i + 1) {
        let offset = step * f32(i);
        sum = sum + textureSample(source, source_sampler, input.uv + offset).rgb * weights[i];
        sum = sum + textureSample(source, source_sampler, input.uv - offset).rgb * weights[i];
    }
    return vec4<f32>(sum, 1.0);
}

// The highlight roll-off.
//
// **Not the ACES fit**, and the reason is worth writing down because that fit
// is the obvious thing to reach for. ACES expects *scene-referred* input, where
// 1.0 is a middling value and real highlights sit far above it. This renderer
// is display-referred: a fully lit white block IS 1.0, and the only things
// above it are emissive. Measured on the fixed scene, the ACES fit maps 0.42 to
// 0.558 and 0.83 to 0.761 — it brightens the midtones and squashes the top, so
// the sky came out washed and nearly grey, and mode 3 looked worse than mode 2
// while claiming to be the beautiful one.
//
// So: identity below the knee, and a soft shoulder above it that approaches 1
// without ever reaching it. Everything mode 2 draws under the knee is pixel for
// pixel what mode 2 draws; what a lamp adds on top has somewhere to go other
// than a hard clip to flat white.
//
// The shoulder is C1-continuous at the knee — its slope there is exactly 1 — so
// there is no visible crease across a surface that brightens through it.
const KNEE: f32 = 0.8;

fn shoulder(x: f32) -> f32 {
    if (x <= KNEE) {
        return x;
    }
    let headroom = 1.0 - KNEE;
    return KNEE + headroom * (1.0 - exp(-(x - KNEE) / headroom));
}

fn tonemap(colour: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(shoulder(colour.r), shoulder(colour.g), shoulder(colour.b));
}

// The last pass: scene plus bloom, exposed and tonemapped, into the swapchain.
@fragment
fn composite_main(input: VertexOut) -> @location(0) vec4<f32> {
    let scene = textureSample(source, source_sampler, input.uv).rgb;
    let glow = textureSample(bloom, source_sampler, input.uv).rgb;
    // Added rather than mixed: bloom is light that scattered on its way to the
    // eye, so it arrives IN ADDITION to what the surface sent. Mixing would
    // dim the surface to make room for its own glow.
    let lit = (scene + glow * post.intensity) * post.exposure;
    return vec4<f32>(tonemap(lit), 1.0);
}
