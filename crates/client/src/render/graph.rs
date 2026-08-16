// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The post-processing chain for lighting mode 3, and the small graph that runs
//! it.
//!
//! # Why a graph at all, when it currently runs four fixed passes
//!
//! Task 11 adds screen-space reflections, and Task 16 will want whatever
//! polish needs. A chain hard-coded into `Renderer::render` means every
//! addition edits the middle of the frame function and re-derives which target
//! is being read and which written. Here a pass is a shader entry point, a
//! source, and a destination, and adding one is adding an entry to a list.
//!
//! It is deliberately NOT a general graph. There is no automatic resource
//! aliasing, no dependency solving, and no lifetime analysis: passes run in the
//! order given, over targets this module owns. That machinery is worth building
//! when the chain is long enough to make a scheduling mistake, and not before.
//!
//! # Nothing here exists in modes 1 and 2
//!
//! Task 10's criterion is that mode 1 keeps Task 08's cost profile, "no
//! shadow/post allocations when in mode 1". [`Post`] is built when mode 3 is
//! selected and dropped when it is left, so the criterion is a property of the
//! code rather than a promise in a comment — `Renderer::post_bytes` reports
//! zero in the other modes, and a test asserts it.
//!
//! # Why the scene is drawn into a float target
//!
//! Bloom needs to know what was brighter than white, and an 8-bit target has
//! already thrown that away: a lamp at four times white and a lamp at exactly
//! white are both `1.0` by the time the pass could look. `Rgba16Float` keeps
//! the headroom, and the composite is what brings it back into display range.

use super::{COLOUR_FORMAT, DEPTH_FORMAT, Gpu};

/// The format the scene is drawn into before tonemapping.
pub const HDR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// How much smaller the bloom buffers are than the frame.
///
/// Half. A blur is a low-pass filter and the result is going to be soft
/// whatever resolution it was computed at, so half resolution costs a quarter
/// of the samples for a difference nobody can point at. Quarter resolution is
/// visibly blocky on a hard-edged glow, which a lamp against a dark wall is.
const BLOOM_DIVISOR: u32 = 2;

/// Luma above which a surface starts to glow, and the width of the transition.
///
/// Above 1.0 on purpose: **only things brighter than white bloom**. A threshold
/// below white makes every lit surface glow, which reads as a dirty lens rather
/// than as light.
const BLOOM_CUTOFF: f32 = 1.0;
const BLOOM_KNEE: f32 = 0.6;

/// How much of the blurred image is added back.
const BLOOM_INTENSITY: f32 = 0.35;

/// What one post pass needs to know.
///
/// Field order matches `post.wgsl` exactly, and nothing checks that it does —
/// the two are one memory layout written down twice. Getting it wrong in
/// `Globals` made the world come out red; the same mistake here would make the
/// fog wrong in a way that reads as "the sky colour is broken".
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    /// Clip back to camera-relative space, for reconstructing where a depth
    /// sample is. Only the composite reads it.
    inverse_view_projection: [[f32; 4]; 4],
    /// Sky colour in `xyz`, where fog is total in `w`.
    sky: [f32; 4],
    /// Sun colour in `xyz`, scattering strength in `w`.
    sun: [f32; 4],
    /// Which way sunlight travels in `xyz`, where fog starts in `w`.
    sun_direction: [f32; 4],
    /// One texel of the source, in UV.
    texel: [f32; 2],
    /// Threshold cutoff and knee, or blur direction.
    params: [f32; 2],
    intensity: f32,
    /// Exposure, applied before the tonemap.
    ///
    /// The sky's, so a mod can open up the frame at dusk and stop it clipping at
    /// noon. It multiplies before the highlight roll-off on purpose — after it,
    /// exposure would only slide an already-compressed picture up and down.
    exposure: f32,
    /// Whether to look the tonemapped colour up in the grading table.
    ///
    /// A flag rather than an always-on lookup, because an eight-bit table of the
    /// identity is not exactly the identity and an ungraded world has to be
    /// untouched — see [`super::grade`].
    graded: f32,
    _pad: f32,
}

/// What the frame's sky is doing, as the composite needs it.
///
/// Passed in per frame rather than stored: the renderer owns these and they
/// change every tick, and a copy here would be a second place for them to be
/// stale.
#[derive(Debug, Clone, Copy)]
pub struct Frame {
    /// Clip back to camera-relative space.
    pub inverse_view_projection: glam::Mat4,
    /// The sky's colour.
    pub sky: [f32; 3],
    /// The sun's colour.
    pub sun: [f32; 3],
    /// Which way its light travels.
    pub sun_direction: [f32; 3],
    /// Where fog begins, in blocks.
    pub fog_start: f32,
    /// Where it is total, in blocks.
    pub fog_end: f32,
    /// How the finished frame is graded, already interpolated and sanitised by
    /// `crate::sky`.
    pub grade: tiamot_core::proto::SkyGrade,
}

/// How much of the sun's colour the haze takes on where the view points at it.
///
/// Enough to see on a hazy horizon, not enough to make the fog a second sun.
const SCATTERING: f32 = 0.6;

/// A colour target and its view.
struct Target {
    view: wgpu::TextureView,
    width: u32,
    height: u32,
    bytes: u64,
}

impl Target {
    fn new(gpu: &Gpu, label: &str, width: u32, height: u32, format: wgpu::TextureFormat) -> Self {
        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: width.max(1),
                height: height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let bytes = u64::from(width.max(1))
            * u64::from(height.max(1))
            * u64::from(format.target_pixel_byte_cost().unwrap_or(4));
        Self {
            view: texture.create_view(&wgpu::TextureViewDescriptor::default()),
            width: width.max(1),
            height: height.max(1),
            bytes,
        }
    }
}

/// The bindings every post pass shares: its uniforms, the source texture, a
/// sampler, and the bloom buffer the composite adds in.
///
/// One layout for all four passes. They do not all read bloom, and the ones
/// that do not bind a 1x1 black texture there — cheaper than a second layout
/// and a second pipeline layout to go with it, and it keeps the shader's
/// bindings in one place.
fn post_bind_layout(gpu: &Gpu) -> wgpu::BindGroupLayout {
    gpu.device
        .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("post-bind-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        // Depth, read with `textureLoad` and therefore never
                        // filtered: the average of two surfaces at different
                        // distances is a distance where neither of them is, and
                        // across a silhouette that is every edge in the frame.
                        sample_type: wgpu::TextureSampleType::Depth,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 5,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        // The grading table. Filterable and filtered: the whole
                        // point of a 16-sample axis is that the hardware
                        // interpolates between entries, and a nearest-sampled
                        // LUT is sixteen visible bands per channel.
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D3,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        })
}

/// One fullscreen pass's pipeline.
///
/// The four differ only in fragment entry point and output format, so they are
/// one function called four times rather than four descriptors to keep in step.
fn post_pipeline(
    gpu: &Gpu,
    shader: &wgpu::ShaderModule,
    layout: &wgpu::PipelineLayout,
    label: &str,
    entry: &str,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    gpu.device
        .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(label),
            layout: Some(layout),
            vertex: wgpu::VertexState {
                module: shader,
                entry_point: Some("vertex_main"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: shader,
                entry_point: Some(entry),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            // No depth anywhere in the chain. Every pass covers the whole
            // target exactly once, so there is nothing to sort.
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        })
}

/// The shaders and layout mode 3 compiles its own pipelines from.
///
/// The same modules the other modes draw with — a second copy compiled from the
/// same file would be a second thing to keep in step.
#[derive(Debug, Clone, Copy)]
pub struct Shaders<'a> {
    /// The world shader.
    pub world: &'a wgpu::ShaderModule,
    /// The selection-outline shader.
    pub selection: &'a wgpu::ShaderModule,
    /// The bind group layout both of them use.
    pub layout: &'a wgpu::BindGroupLayout,
}

/// What the chain is being built for.
#[derive(Debug, Clone, Copy)]
pub struct Setup {
    /// Textured, flat or wireframe — the debugging axis, which mode 3's own
    /// pipelines have to honour like everyone else's.
    pub mode: super::RenderMode,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Texels per shadow cascade, or `None` for no shadows.
    pub shadow_texels: Option<u32>,
}

/// One pass of the chain, as the graph describes it.
struct Step<'a> {
    label: &'a str,
    pipeline: &'a wgpu::RenderPipeline,
    /// Which uniform buffer carries this pass's parameters.
    slot: usize,
    source: &'a wgpu::TextureView,
    /// The blurred image, for the pass that adds it back. `None` binds black,
    /// because a binding cannot be left empty and binding the target being
    /// written is a hazard the validator rejects.
    bloom: Option<&'a wgpu::TextureView>,
    target: &'a wgpu::TextureView,
}

/// The world pipeline for the float target, with or without cascades to bind.
///
/// `fragment_main` does not mention the shadow bind group and `fragment_shadowed`
/// does, so the two pipelines have different layouts — which is what lets
/// "shadows off" allocate no cascades at all rather than binding empty ones.
fn world_pipeline_for(
    gpu: &Gpu,
    shader: &wgpu::ShaderModule,
    layout: &wgpu::BindGroupLayout,
    shadows: Option<&super::shadow::Shadows>,
    mode: crate::config::RenderMode,
) -> wgpu::RenderPipeline {
    match shadows {
        Some(shadows) => super::build_shadowed_pipeline(
            gpu,
            shader,
            layout,
            shadows.sample_layout(),
            mode,
            HDR_FORMAT,
        ),
        None => super::build_pipeline(gpu, shader, layout, mode, HDR_FORMAT),
    }
}

/// The mode 3 chain: an HDR scene target, two bloom buffers, and the pipelines
/// that walk between them.
pub struct Post {
    /// The scene, in float, before anything is done to it.
    scene: Target,
    /// Its depth buffer. Mode 3 cannot share the renderer's, which is sized to
    /// the swapchain and cleared by the direct path.
    depth: wgpu::TextureView,
    /// Ping and pong for the separable blur, at [`BLOOM_DIVISOR`] scale.
    bloom: [Target; 2],
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// One pipeline per entry point. They differ only in fragment entry and
    /// output format, and are built once rather than per frame.
    threshold: wgpu::RenderPipeline,
    blur: wgpu::RenderPipeline,
    composite: wgpu::RenderPipeline,
    /// A uniform buffer per pass. Separate buffers rather than one with dynamic
    /// offsets: four passes is not enough traffic to be worth the alignment
    /// rules, and a wrong offset is an invisible bug rather than a loud one.
    uniforms: [wgpu::Buffer; 4],
    /// A 1x1 black texture, bound as `bloom` in the passes that have no bloom
    /// to read. A binding cannot be left empty, and binding the target being
    /// written would be a read-write hazard the validator rejects.
    black: wgpu::TextureView,
    /// The time-of-day grading table. Bound by every pass and read by the
    /// composite alone, which is cheaper than a second bind layout for the sake
    /// of three passes that ignore it.
    grading: super::grade::Grading,
    size: (u32, u32),
    /// The cascades. Owned here so that everything mode 3 allocates is built
    /// and dropped as one thing, and `None` when shadows are turned off — which
    /// is a setting of its own rather than a reason to leave mode 3.
    shadows: Option<super::shadow::Shadows>,
    /// The world and selection pipelines again, built for the float target.
    ///
    /// A pipeline is compiled against one output format, so mode 3 cannot reuse
    /// the pipelines that write the swapchain. They live here rather than
    /// beside them so that everything mode 3 allocates is dropped together —
    /// two `Option`s that had to be kept in step would eventually not be.
    world: wgpu::RenderPipeline,
    fluid: wgpu::RenderPipeline,
    selection: wgpu::RenderPipeline,
}

impl Post {
    /// Builds everything mode 3 needs for a frame of this size.
    ///
    /// The world and selection shaders come in rather than being loaded here:
    /// they are the SAME shaders the other modes draw with, and a second copy
    /// compiled from the same file would be a second thing to keep in step.
    #[must_use]
    pub fn new(gpu: &Gpu, shaders: &Shaders<'_>, frame: Setup) -> Self {
        let Shaders {
            world: world_shader,
            selection: selection_shader,
            layout: world_layout,
        } = *shaders;
        let Setup {
            mode,
            width,
            height,
            shadow_texels,
        } = frame;
        let shader = gpu
            .device
            .create_shader_module(wgpu::include_wgsl!("post.wgsl"));

        let layout = post_bind_layout(gpu);

        let pipeline_layout = gpu
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("post-pipeline-layout"),
                bind_group_layouts: &[Some(&layout)],
                immediate_size: 0,
            });

        let build = |label: &str, entry: &str, format: wgpu::TextureFormat| {
            post_pipeline(gpu, &shader, &pipeline_layout, label, entry, format)
        };

        let uniform = |label: &str| {
            gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: size_of::<Uniforms>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        };

        let black = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("post-black"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: HDR_FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });

        let bloom_width = (width / BLOOM_DIVISOR).max(1);
        let bloom_height = (height / BLOOM_DIVISOR).max(1);
        let shadows = shadow_texels.map(|size| super::shadow::Shadows::new(gpu, size));

        Self {
            scene: Target::new(gpu, "post-scene", width, height, HDR_FORMAT),
            depth: super::make_sampled_depth(gpu, width.max(1), height.max(1)),
            bloom: [
                Target::new(gpu, "post-bloom-a", bloom_width, bloom_height, HDR_FORMAT),
                Target::new(gpu, "post-bloom-b", bloom_width, bloom_height, HDR_FORMAT),
            ],
            threshold: build("post-threshold", "threshold_main", HDR_FORMAT),
            blur: build("post-blur", "blur_main", HDR_FORMAT),
            composite: build("post-composite", "composite_main", COLOUR_FORMAT),
            uniforms: [
                uniform("post-threshold-uniforms"),
                uniform("post-blur-h-uniforms"),
                uniform("post-blur-v-uniforms"),
                uniform("post-composite-uniforms"),
            ],
            world: world_pipeline_for(gpu, world_shader, world_layout, shadows.as_ref(), mode),
            fluid: super::build_fluid_pipeline(
                gpu,
                world_shader,
                &[Some(world_layout)],
                HDR_FORMAT,
            ),
            selection: super::build_selection_pipeline(
                gpu,
                selection_shader,
                world_layout,
                HDR_FORMAT,
            ),
            shadows,
            grading: super::grade::Grading::new(gpu),
            black: black.create_view(&wgpu::TextureViewDescriptor::default()),
            sampler: gpu.device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("post-sampler"),
                // Clamped, and it matters: a blur reads past the edge, and
                // repeating there wraps the far side of the screen into the
                // near one — a bright window on the left glowing on the right.
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                address_mode_w: wgpu::AddressMode::ClampToEdge,
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            }),
            layout,
            size: (width.max(1), height.max(1)),
        }
    }

    /// The view the world pass draws into, and the depth to go with it.
    #[must_use]
    pub const fn scene_target(&self) -> (&wgpu::TextureView, &wgpu::TextureView) {
        (&self.scene.view, &self.depth)
    }

    /// The cascades, for updating them and for drawing into them.
    #[must_use]
    pub const fn shadows(&self) -> Option<&super::shadow::Shadows> {
        self.shadows.as_ref()
    }

    /// The cascades, mutably, so the frame can move them with the camera.
    pub const fn shadows_mut(&mut self) -> Option<&mut super::shadow::Shadows> {
        self.shadows.as_mut()
    }

    /// Texels per cascade, or `None` when shadows are off — so the frame can
    /// tell whether the chain it has matches the setting it was asked for.
    #[must_use]
    pub fn shadow_texels(&self) -> Option<u32> {
        self.shadows.as_ref().map(super::shadow::Shadows::size)
    }

    /// The world pipeline, compiled for the float target.
    #[must_use]
    pub const fn world_pipeline(&self) -> &wgpu::RenderPipeline {
        &self.world
    }

    /// The blended fluid pipeline, compiled for the float target.
    #[must_use]
    pub const fn fluid_pipeline(&self) -> &wgpu::RenderPipeline {
        &self.fluid
    }

    /// The selection-outline pipeline, compiled for the float target.
    #[must_use]
    pub const fn selection_pipeline(&self) -> &wgpu::RenderPipeline {
        &self.selection
    }

    /// Whether this chain is built for a frame of this size.
    #[must_use]
    pub const fn fits(&self, width: u32, height: u32) -> bool {
        self.size.0 == width && self.size.1 == height
    }

    /// How much texture memory the chain is holding.
    ///
    /// For the HUD and for the test that mode 1 allocates none of it.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.scene.bytes
            + self.bloom[0].bytes
            + self.bloom[1].bytes
            + super::grade::BYTES
            + self
                .shadows
                .as_ref()
                .map_or(0, super::shadow::Shadows::bytes)
    }

    /// Re-bakes the grading table if the sky has moved far enough to matter.
    ///
    /// Separate from [`Post::run`] and called before it, because baking uploads
    /// a texture and `run` takes `&self` — and because the frame that decides
    /// what the grade is should be the frame that pays for it.
    ///
    /// Returns whether anything was uploaded, for the test that a still sky
    /// bakes once rather than every frame.
    pub fn bake_grade(&mut self, gpu: &Gpu, grade: &tiamot_core::proto::SkyGrade) -> bool {
        self.grading.bake(gpu, grade)
    }

    /// Runs threshold, blur, blur, composite — scene in, `target` out.
    ///
    /// The order is the whole graph. Each pass names what it reads and what it
    /// writes, and nothing else in the renderer needs to know the chain exists.
    pub fn run(
        &self,
        gpu: &Gpu,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        frame: &Frame,
    ) {
        let full_texel = [1.0 / self.size.0 as f32, 1.0 / self.size.1 as f32];
        // The BLOOM buffer's texel, not the frame's. Stepping a blur by a
        // full-resolution texel over a half-resolution source samples the same
        // texel nine times and blurs nothing at all.
        let bloom_texel = [
            1.0 / self.bloom[0].width as f32,
            1.0 / self.bloom[0].height as f32,
        ];

        // The chain, in order: bright parts out of the scene, blurred across,
        // blurred down, then added back over the scene and tonemapped. Adding
        // a pass — Task 11's reflections, say — is adding an entry here.
        self.write(gpu, 0, full_texel, [BLOOM_CUTOFF, BLOOM_KNEE], frame);
        self.write(gpu, 1, bloom_texel, [1.0, 0.0], frame);
        self.write(gpu, 2, bloom_texel, [0.0, 1.0], frame);
        self.write(gpu, 3, full_texel, [0.0, 0.0], frame);

        for step in [
            Step {
                label: "post-threshold",
                pipeline: &self.threshold,
                slot: 0,
                source: &self.scene.view,
                bloom: None,
                target: &self.bloom[0].view,
            },
            Step {
                label: "post-blur-h",
                pipeline: &self.blur,
                slot: 1,
                source: &self.bloom[0].view,
                bloom: None,
                target: &self.bloom[1].view,
            },
            Step {
                label: "post-blur-v",
                pipeline: &self.blur,
                slot: 2,
                source: &self.bloom[1].view,
                bloom: None,
                target: &self.bloom[0].view,
            },
            Step {
                label: "post-composite",
                pipeline: &self.composite,
                slot: 3,
                source: &self.scene.view,
                bloom: Some(&self.bloom[0].view),
                target,
            },
        ] {
            self.step(gpu, encoder, &step);
        }
    }

    fn write(&self, gpu: &Gpu, slot: usize, texel: [f32; 2], params: [f32; 2], frame: &Frame) {
        gpu.queue.write_buffer(
            &self.uniforms[slot],
            0,
            bytemuck::bytes_of(&Uniforms {
                inverse_view_projection: frame.inverse_view_projection.to_cols_array_2d(),
                sky: [frame.sky[0], frame.sky[1], frame.sky[2], frame.fog_end],
                sun: [frame.sun[0], frame.sun[1], frame.sun[2], SCATTERING],
                sun_direction: [
                    frame.sun_direction[0],
                    frame.sun_direction[1],
                    frame.sun_direction[2],
                    frame.fog_start,
                ],
                texel,
                params,
                intensity: BLOOM_INTENSITY,
                exposure: frame.grade.exposure,
                // The identity is skipped rather than looked up. See
                // [`super::grade`] for why "nearly unchanged" is not good
                // enough for an ungraded world.
                graded: f32::from(u8::from(frame.grade != tiamot_core::proto::SkyGrade::NONE)),
                _pad: 0.0,
            }),
        );
    }

    /// One entry in the chain: what to run, what it reads, where it lands.
    ///
    /// A struct rather than six arguments because that is what a graph node is,
    /// and because two of the six are texture views that would otherwise be
    /// easy to swap by accident — a blur that reads its own output is a bug
    /// with no error message.
    fn step(&self, gpu: &Gpu, encoder: &mut wgpu::CommandEncoder, step: &Step<'_>) {
        let bind = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(step.label),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.uniforms[step.slot].as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(step.source),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(step.bloom.unwrap_or(&self.black)),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::TextureView(&self.depth),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: wgpu::BindingResource::TextureView(self.grading.view()),
                },
            ],
        });

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some(step.label),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: step.target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    // Cleared rather than loaded: every pass covers its whole
                    // target, so loading would be reading a surface that is
                    // about to be entirely overwritten.
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(step.pipeline);
        pass.set_bind_group(0, &bind, &[]);
        pass.draw(0..3, 0..1);
    }
}

/// The depth format the scene target uses, restated so a reader of this module
/// does not have to go and look.
const _: () = assert!(matches!(DEPTH_FORMAT, wgpu::TextureFormat::Depth32Float));
