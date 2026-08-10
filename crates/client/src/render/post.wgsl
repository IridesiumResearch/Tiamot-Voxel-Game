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
    // Clip back to camera-relative space, for turning a depth sample into a
    // position. Task 11's reflections need exactly the same reconstruction,
    // which is why this is a matrix rather than a pair of plane distances.
    inverse_view_projection: mat4x4<f32>,
    // Sky colour in xyz, where fog becomes total in w.
    sky: vec4<f32>,
    // Sun colour in xyz, how strong the scattering is in w.
    sun: vec4<f32>,
    // Which way sunlight travels in xyz, where fog starts in w.
    sun_direction: vec4<f32>,
    // One texel of the SOURCE texture, in UV. The blur steps along it, so a
    // blur reading a half-resolution source with a full-resolution texel size
    // samples the same texel nine times and does nothing at all.
    texel: vec2<f32>,
    // Pass-specific: the threshold's cutoff and knee, or the blur's direction.
    params: vec2<f32>,
    // How much bloom to add back in the composite.
    intensity: f32,
    // Exposure applied before the tonemap. The sky's, so a mod can open the
    // frame up at dusk and keep it from clipping at noon.
    exposure: f32,
    // Above 0.5, look the tonemapped colour up in `grade_lut`. Off for a sky
    // that grades nothing, because an eight-bit table of the identity is not
    // exactly the identity — see `render::grade`.
    graded: f32,
    _pad: f32,
};

@group(0) @binding(0) var<uniform> post: Post;
@group(0) @binding(1) var source: texture_2d<f32>;
@group(0) @binding(2) var source_sampler: sampler;
@group(0) @binding(3) var bloom: texture_2d<f32>;
// The scene's depth, for fog that knows how far away things are. Read with
// `textureLoad` at integer coordinates rather than sampled: depth is not a
// colour, filtering it averages across silhouette edges, and the average of two
// surfaces at different distances is a distance where neither of them is.
@group(0) @binding(4) var scene_depth: texture_depth_2d;
// The time-of-day grading table: a colour cube the composite looks its finished
// pixels up in. sRGB-encoded, so this sample comes back linear. Bound by every
// pass and read by the composite alone.
@group(0) @binding(5) var grade_lut: texture_3d<f32>;

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

// How far the surface under this pixel is, in blocks.
//
// Reconstructed from depth rather than carried through from the world pass,
// because the fog has to apply to the SKY as well and the sky is a clear colour
// with no geometry behind it. At the far plane this comes back enormous, which
// is exactly right: sky is infinitely far away and infinitely fogged, and since
// the fog colour is the sky colour that is a no-op except where the scattering
// tints it.
fn scene_distance(pixel: vec2<i32>, uv: vec2<f32>) -> f32 {
    let depth = textureLoad(scene_depth, pixel, 0);
    // Clip space: x and y across the viewport, z the depth as written.
    let clip = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, depth, 1.0);
    let view = post.inverse_view_projection * clip;
    // The camera is the origin (floating origin), so the reconstructed point IS
    // the offset from the eye and its length is the distance.
    return length(view.xyz / view.w);
}

// Sunlight scattered toward the eye by the air between it and the surface.
//
// A cheap approximation of what makes a hazy afternoon glow around the sun:
// the closer the view ray is to pointing at the sun, the more of the sun's own
// colour the haze takes on. Not a physical model of scattering — Task 10 asks
// for stylised — but it has the property that matters, which is that fog is not
// one flat colour across the whole sky.
fn scattered_fog(uv: vec2<f32>) -> vec3<f32> {
    // The view ray for this pixel, from the same reconstruction the distance
    // uses, at the far plane.
    let clip = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 1.0, 1.0);
    let far = post.inverse_view_projection * clip;
    let ray = normalize(far.xyz / far.w);

    // Sunlight travels along `sun_direction`, so looking INTO the sun means
    // looking along its negation.
    let toward_sun = max(dot(ray, -post.sun_direction.xyz), 0.0);
    // Raised to a power so the glow is a halo around the sun rather than half
    // the sky. Squared twice: cheap, and no `pow` for a shape nobody measures.
    let halo = toward_sun * toward_sun * toward_sun * toward_sun;
    return mix(post.sky.rgb, post.sun.rgb, halo * post.sun.w);
}

// How many samples per axis the grading table has. Must match
// `render::grade::SIZE`; a mismatch is a subtle contrast error, not a failure.
const GRADE_SIZE: f32 = 16.0;

// The time-of-day grade: one trilinear lookup.
//
// **The coordinate scaling is the whole trick and the classic place to get it
// wrong.** A texture coordinate of 0 lands on the *edge* of the first texel, not
// its centre, so feeding the colour in raw samples half a texel outside the
// table at both ends and clamps there — which shows up as slightly crushed
// blacks and whites that no amount of staring at the grade explains. The table's
// first entry sits at 0.5/N and its last at (N-0.5)/N, so a colour of 0..1 maps
// onto that range.
fn graded(colour: vec3<f32>) -> vec3<f32> {
    let scale = (GRADE_SIZE - 1.0) / GRADE_SIZE;
    let offset = 0.5 / GRADE_SIZE;
    let uvw = clamp(colour, vec3<f32>(0.0), vec3<f32>(1.0)) * scale + offset;
    return textureSample(grade_lut, source_sampler, uvw).rgb;
}

// The last pass: scene plus bloom, fogged, exposed, tonemapped and graded.
@fragment
fn composite_main(input: VertexOut) -> @location(0) vec4<f32> {
    let scene = textureSample(source, source_sampler, input.uv).rgb;
    let glow = textureSample(bloom, source_sampler, input.uv).rgb;
    // Added rather than mixed: bloom is light that scattered on its way to the
    // eye, so it arrives IN ADDITION to what the surface sent. Mixing would
    // dim the surface to make room for its own glow.
    let lit = (scene + glow * post.intensity) * post.exposure;

    // Fog here rather than in the world shader, for mode 3 only. Doing it from
    // depth is what lets it reach the sky and take the sun's colour with it;
    // the world shader's own fog is per-surface and cannot do either. The world
    // shader skips its fog in this mode so the two do not stack.
    let distance = scene_distance(vec2<i32>(input.clip.xy), input.uv);
    let start = post.sun_direction.w;
    let haze = clamp((distance - start) / max(post.sky.w - start, 0.001), 0.0, 1.0);
    let fogged = mix(lit, scattered_fog(input.uv), haze);

    // Graded last, on the display-referred result. The table's domain is 0..1
    // and this is where the frame first lives in it: grading before the tonemap
    // would ask the table about values above white, which it has no entries for.
    //
    // Sampled unconditionally and selected between, rather than sampled inside
    // an `if`: the condition is uniform so a branch would be legal, but a select
    // needs no argument about that and costs one fetch in the pass that runs
    // once per pixel per frame.
    let display = tonemap(fogged);
    return vec4<f32>(select(display, graded(display), post.graded > 0.5), 1.0);
}
