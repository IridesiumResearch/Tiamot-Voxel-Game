// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Drawing figures: the engine's humanoid, posed by its own skeleton.
//!
//! # Why this is a second pipeline and not a second mesh
//!
//! The world's vertex is eight bytes and snaps to a sub-node **cell** — six
//! bits an axis. That is exactly right for voxels and useless for a person: a
//! humanoid is 5.4 cells tall and 1.8 wide, so a head quantised to a cell is
//! one cube. Every entity in the game was drawn as its collision box until this
//! existed, for that reason and no other.
//!
//! So a skinned vertex carries float positions, a normal, four joint indices
//! and four weights, and the vertex stage moves it by the matrices its joints
//! hold this frame. Different format, different shader, different pipeline.
//!
//! # One buffer of matrices for the whole frame
//!
//! Every figure's palette is written end to end into one storage buffer and
//! each instance says where its own begins. The alternative — a uniform buffer
//! per figure, or a bind group per figure — is a bind per draw, and two hundred
//! mobs is two hundred binds. This way it is one bind and two hundred draws
//! that differ only by an instance index, which is what an instance is for.
//!
//! # What is deliberately not here
//!
//! Figures do not sample the shadow map. They cast into it — a mob with no
//! shadow floats — but they are lit by the sun, the ambient and the fog and
//! nothing else. Reading cascades on a moving body is a self-shadowing problem
//! (an arm across a chest, at a bias tuned for terrain) that belongs after
//! somebody has looked at one.

use tiamot_core::model::{self, Model};
use wgpu::util::DeviceExt as _;

use super::{DEPTH_FORMAT, Gpu, RenderMode};

/// A figure to draw this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Figure {
    /// Where its feet are, camera-relative, in blocks — the floating origin
    /// every other draw here uses (charter rule 7).
    pub offset: [f32; 3],
    /// Which way it faces, in radians.
    pub yaw: f32,
    /// The server's state tag, which picks the clip.
    pub anim: u8,
    /// Seconds into that clip.
    ///
    /// The caller's job, and it carries a per-entity offset: two hundred mobs
    /// sharing a clock march in step, which reads as a chorus line rather than
    /// as a crowd.
    pub phase: f32,
}

/// One instance, as the shader reads it.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Instance {
    offset: [f32; 4],
    /// Heading in x, the palette's first matrix in y (bit-cast from `u32`).
    /// Two spare, because a vertex attribute is a `vec4` either way.
    placement: [f32; 4],
}

/// Everything needed to draw skinned figures.
pub struct Skinned {
    vertices: wgpu::Buffer,
    indices: wgpu::Buffer,
    index_count: u32,
    /// The model the palettes are built from. One for now — the engine's — and
    /// the field rather than a constant because mod-supplied models arrive
    /// through the same path.
    model: Model,
    /// Per-frame instances, grown when a frame needs more.
    instances: wgpu::Buffer,
    instance_capacity: usize,
    drawn: usize,
    /// Every figure's joint matrices, end to end.
    palette: wgpu::Buffer,
    palette_capacity: usize,
    palette_layout: wgpu::BindGroupLayout,
    palette_bind: wgpu::BindGroup,
    shader: wgpu::ShaderModule,
}

/// Figures a frame can draw before the buffers grow.
///
/// Sized for the task's own gate of two hundred mobs plus the players among
/// them, so the common case never reallocates.
const INITIAL_FIGURES: usize = 256;

/// Matrices in one figure's palette, for the initial allocation.
const INITIAL_JOINTS: usize = 24;

impl Skinned {
    /// Uploads a model and builds everything that draws it.
    pub fn new(gpu: &Gpu, model: Model) -> Self {
        let vertices: Vec<GpuVertex> = model.vertices.iter().map(GpuVertex::from).collect();
        let vertex_buffer = gpu
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("skinned-vertices"),
                contents: bytemuck::cast_slice(&vertices),
                usage: wgpu::BufferUsages::VERTEX,
            });
        let index_buffer = gpu
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("skinned-indices"),
                contents: bytemuck::cast_slice(&model.indices),
                usage: wgpu::BufferUsages::INDEX,
            });

        let instances = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("skinned-instances"),
            size: (INITIAL_FIGURES * size_of::<Instance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let palette_capacity = INITIAL_FIGURES * INITIAL_JOINTS;
        let palette = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("skinned-palette"),
            size: (palette_capacity * size_of::<[f32; 16]>()) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let palette_layout = palette_layout(gpu);
        let palette_bind = palette_bind(gpu, &palette_layout, &palette);

        let shader = gpu
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("skinned"),
                source: wgpu::ShaderSource::Wgsl(include_str!("skinned.wgsl").into()),
            });

        Self {
            vertices: vertex_buffer,
            indices: index_buffer,
            index_count: u32::try_from(model.indices.len()).unwrap_or(0),
            model,
            instances,
            instance_capacity: INITIAL_FIGURES,
            drawn: 0,
            palette,
            palette_capacity,
            palette_layout,
            palette_bind,
            shader,
        }
    }

    /// The bind group layout the pipelines need for the palette.
    #[must_use]
    pub const fn layout(&self) -> &wgpu::BindGroupLayout {
        &self.palette_layout
    }

    /// The shader, so a caller can compile it against its own target format.
    #[must_use]
    pub const fn shader(&self) -> &wgpu::ShaderModule {
        &self.shader
    }

    /// How many joints one figure's palette takes.
    #[must_use]
    pub fn joints(&self) -> usize {
        self.model.skin.joints.len()
    }

    /// Poses every figure and writes the frame's buffers.
    ///
    /// Called once per frame, before any pass draws: all three passes read the
    /// same instances and the same palette, which is what makes a figure appear
    /// in the same place in the world and in its own shadow.
    pub fn prepare(&mut self, gpu: &Gpu, figures: &[Figure]) {
        self.drawn = figures.len();
        if figures.is_empty() {
            return;
        }

        let joints = self.joints();
        if joints == 0 {
            self.drawn = 0;
            return;
        }

        let mut instances = Vec::with_capacity(figures.len());
        let mut matrices: Vec<f32> = Vec::with_capacity(figures.len() * joints * 16);
        for figure in figures {
            let base = u32::try_from(matrices.len() / 16).unwrap_or(0);
            let clip = self
                .model
                .clip(model::clip_for(tiamot_core::ent::AnimTag(figure.anim)));
            for matrix in model::skinning_matrices(&self.model, clip, figure.phase) {
                matrices.extend_from_slice(&matrix);
            }
            instances.push(Instance {
                offset: [figure.offset[0], figure.offset[1], figure.offset[2], 0.0],
                // The base index is an integer carried through a float
                // attribute, so it is bit-cast rather than converted: at more
                // than sixteen million matrices a conversion would start
                // rounding, and a palette index that rounds draws somebody
                // else's arm.
                placement: [figure.yaw, f32::from_bits(base), 0.0, 0.0],
            });
        }

        self.grow(gpu, instances.len(), matrices.len() / 16);
        gpu.queue
            .write_buffer(&self.instances, 0, bytemuck::cast_slice(&instances));
        gpu.queue
            .write_buffer(&self.palette, 0, bytemuck::cast_slice(&matrices));
    }

    /// Reallocates if this frame needs more room than the last one had.
    fn grow(&mut self, gpu: &Gpu, figures: usize, matrices: usize) {
        if figures > self.instance_capacity {
            // Doubled rather than fitted, so a world that keeps gaining mobs
            // reallocates a handful of times rather than every frame.
            self.instance_capacity = figures.next_power_of_two();
            self.instances = gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("skinned-instances"),
                size: (self.instance_capacity * size_of::<Instance>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        if matrices > self.palette_capacity {
            self.palette_capacity = matrices.next_power_of_two();
            self.palette = gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("skinned-palette"),
                size: (self.palette_capacity * size_of::<[f32; 16]>()) as u64,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            // The bind group names a buffer, so a new buffer needs a new one.
            self.palette_bind = palette_bind(gpu, &self.palette_layout, &self.palette);
        }
    }

    /// Draws every figure prepared this frame.
    ///
    /// The caller has already set the pipeline and whatever group 0 or 1 that
    /// pipeline wants; this binds the palette and the geometry.
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        self.draw_first(pass, self.drawn);
    }

    /// Draws the first `count` figures.
    ///
    /// **For the one figure that is in the world without being visible in it:
    /// the player's own.** In first person their body still blocks the sun and
    /// still stands on the ground, so the cascades and the blob need it — but
    /// drawing it would fill the screen with the inside of a head. It is placed
    /// last, so the world pass asks for one fewer than the shadow pass does.
    pub fn draw_first(&self, pass: &mut wgpu::RenderPass<'_>, count: usize) {
        let count = count.min(self.drawn);
        if count == 0 || self.index_count == 0 {
            return;
        }
        pass.set_bind_group(2, &self.palette_bind, &[]);
        pass.set_vertex_buffer(0, self.vertices.slice(..));
        pass.set_vertex_buffer(1, self.instances.slice(..));
        pass.set_index_buffer(self.indices.slice(..), wgpu::IndexFormat::Uint32);
        let count = u32::try_from(count).unwrap_or(0);
        pass.draw_indexed(0..self.index_count, 0, 0..count);
    }

    /// How many figures the last [`Skinned::prepare`] accepted.
    #[must_use]
    pub const fn drawn(&self) -> usize {
        self.drawn
    }
}

/// A vertex as the GPU takes it.
///
/// `u32` joints rather than `u8`, because a `Uint8x4` attribute arrives in the
/// shader as a `vec4<u32>` anyway and the four extra bytes per vertex buy a
/// format that every backend agrees about.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct GpuVertex {
    position: [f32; 3],
    normal: [f32; 3],
    uv: [f32; 2],
    joints: [u32; 4],
    weights: [f32; 4],
}

impl From<&model::Vertex> for GpuVertex {
    fn from(vertex: &model::Vertex) -> Self {
        Self {
            position: vertex.position,
            normal: vertex.normal,
            uv: vertex.uv,
            joints: vertex.joints.map(u32::from),
            weights: vertex.weights,
        }
    }
}

/// The vertex and instance layouts, matching `skinned.wgsl`.
fn vertex_layout() -> [wgpu::VertexBufferLayout<'static>; 2] {
    const VERTEX: [wgpu::VertexAttribute; 5] = wgpu::vertex_attr_array![
        0 => Float32x3,
        1 => Float32x3,
        2 => Float32x2,
        3 => Uint32x4,
        4 => Float32x4,
    ];
    const INSTANCE: [wgpu::VertexAttribute; 2] = wgpu::vertex_attr_array![
        5 => Float32x4,
        6 => Float32x4,
    ];
    [
        wgpu::VertexBufferLayout {
            array_stride: size_of::<GpuVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &VERTEX,
        },
        wgpu::VertexBufferLayout {
            array_stride: size_of::<Instance>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &INSTANCE,
        },
    ]
}

fn palette_layout(gpu: &Gpu) -> wgpu::BindGroupLayout {
    gpu.device
        .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("skinned-palette-layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: true },
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        })
}

fn palette_bind(
    gpu: &Gpu,
    layout: &wgpu::BindGroupLayout,
    buffer: &wgpu::Buffer,
) -> wgpu::BindGroup {
    gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("skinned-palette"),
        layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: buffer.as_entire_binding(),
        }],
    })
}

/// The pipeline that draws figures into a colour target.
///
/// `globals` is group 0 and the palette is group 2; group 1 is left empty,
/// which is where the world's shadow map lives and which figures do not sample.
#[must_use]
pub fn colour_pipeline(
    gpu: &Gpu,
    skinned: &Skinned,
    globals: &wgpu::BindGroupLayout,
    mode: RenderMode,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    let layout = gpu
        .device
        .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("skinned-layout"),
            bind_group_layouts: &[Some(globals), None, Some(skinned.layout())],
            immediate_size: 0,
        });

    let wireframe = mode == RenderMode::Wireframe && gpu.polygon_mode_line;
    gpu.device
        .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("skinned"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: skinned.shader(),
                entry_point: Some("vertex_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &vertex_layout(),
            },
            fragment: Some(wgpu::FragmentState {
                module: skinned.shader(),
                entry_point: Some("fragment_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: if wireframe {
                    None
                } else {
                    Some(wgpu::Face::Back)
                },
                polygon_mode: if wireframe {
                    wgpu::PolygonMode::Line
                } else {
                    wgpu::PolygonMode::Fill
                },
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Less),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        })
}

/// The pipeline that draws figures into a shadow cascade: depth only.
///
/// The cascade's matrix is group 1 here rather than group 0, so one shader
/// module can hold both entry points — see `skinned.wgsl`.
#[must_use]
pub fn shadow_pipeline(
    gpu: &Gpu,
    skinned: &Skinned,
    cascade: &wgpu::BindGroupLayout,
) -> wgpu::RenderPipeline {
    let layout = gpu
        .device
        .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("skinned-shadow-layout"),
            bind_group_layouts: &[None, Some(cascade), Some(skinned.layout())],
            immediate_size: 0,
        });

    gpu.device
        .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("skinned-shadow"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: skinned.shader(),
                entry_point: Some("vertex_shadow"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &vertex_layout(),
            },
            fragment: None,
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                // No culling: a shadow caster's back faces are what a
                // front-face-culled depth pass would keep, and a figure is thin
                // enough that dropping either half loses limbs from its shadow.
                cull_mode: None,
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Less),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        })
}
