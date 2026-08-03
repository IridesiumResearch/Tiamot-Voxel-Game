// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The wgpu renderer.
//!
//! # Shape of a frame
//!
//! One pipeline, one bind group, one draw call per visible chunk. Chunk meshes
//! live in their own vertex and index buffers, and the camera-relative offsets
//! of everything visible go into a single instance buffer rewritten once per
//! frame — so a moving camera costs one buffer write rather than one per chunk.
//!
//! # Floating origin lives in the instance buffer
//!
//! Charter rule 7. Vertex positions are chunk-local sub-node coordinates in
//! `0..=48`, which `f32` represents exactly, and the only per-chunk float is the
//! offset from the camera — computed in `f64` and narrowed once the magnitude
//! is already small (see [`crate::camera`]). Nothing in this module ever sees a
//! world coordinate, which is what makes rendering at the edge of the world
//! numerically identical to rendering at the origin.
//!
//! # Headless is not a special case
//!
//! [`Renderer`] draws into a [`wgpu::TextureView`] and does not know where it
//! came from. A window supplies a surface texture; [`Offscreen`] supplies one
//! backed by a texture it can read back. The screenshot tests therefore
//! exercise the same code the window does, which is the only way a screenshot
//! test is worth having.

pub mod frustum;
pub mod offscreen;

use std::collections::BTreeMap;

use tiamot_core::ChunkPos;
use wgpu::util::DeviceExt as _;

use crate::camera::Camera;
use crate::config::RenderMode;
use crate::mesher::{Mesh, PackedVertex};
use crate::texture::Atlas;

pub use frustum::Frustum;
pub use offscreen::Offscreen;

/// The colour format everything renders into.
///
/// `Rgba8UnormSrgb` rather than `Bgra8`: it is the format an offscreen target
/// and a surface can both provide on every backend, so the screenshot tests and
/// the window are not looking at differently-encoded pixels.
pub const COLOUR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// The depth format.
pub const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

/// The sky colour a frame is cleared to.
///
/// A recognisable blue rather than black: an empty frame and a broken frame
/// look identical against black, and "the screen is dark" is the least
/// actionable bug report there is.
pub const SKY: wgpu::Color = wgpu::Color {
    r: 0.42,
    g: 0.60,
    b: 0.83,
    a: 1.0,
};

/// Anything that stops the renderer starting.
#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    /// No GPU adapter could be found.
    #[error(
        "no graphics adapter available: {0}. This build needs Vulkan, Metal, DX12, or GL. On a \
         headless machine, install a software Vulkan driver (Mesa's lavapipe) or run the server \
         binary instead — it needs no GPU at all."
    )]
    NoAdapter(String),

    /// The adapter refused to create a device.
    ///
    /// Distinct from having no adapter: this is a device that exists and could
    /// not be used, which usually means a driver problem rather than a missing
    /// one.
    #[error("the graphics adapter `{adapter}` refused to create a device: {reason}")]
    NoDevice {
        /// Which adapter.
        adapter: String,
        /// What it said.
        reason: String,
    },

    /// A texture could not be read back.
    #[error("could not read the rendered frame back from the GPU: {0}")]
    Readback(String),
}

/// Uniforms shared by every draw.
///
/// `repr(C)` and `Pod`: this is memcpy'd to the GPU, and a compiler-chosen
/// layout would put the fields somewhere the shader is not looking.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Globals {
    view_projection: [[f32; 4]; 4],
    atlas_grid: u32,
    atlas_side: u32,
    tile: u32,
    padding: u32,
    render_mode: u32,
    pad: [u32; 3],
}

/// One chunk's camera-relative offset, as an instance attribute.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Instance {
    offset: [f32; 4],
}

/// A chunk's mesh, on the GPU.
struct ChunkMesh {
    vertices: wgpu::Buffer,
    indices: wgpu::Buffer,
    index_count: u32,
}

/// The device, queue, and what we know about the adapter.
///
/// Separate from [`Renderer`] because the window needs an adapter before it can
/// choose a surface configuration, and a renderer that created its own device
/// would have to be built after the window rather than beside it.
pub struct Gpu {
    /// The logical device.
    pub device: wgpu::Device,
    /// Its queue.
    pub queue: wgpu::Queue,
    /// A human-readable adapter name, for the HUD and for screenshot goldens.
    pub adapter: String,
    /// The backend in use, e.g. `Vulkan`.
    pub backend: String,
    /// Whether the device can draw in wireframe.
    pub polygon_mode_line: bool,
}

impl Gpu {
    /// Creates a device, optionally compatible with a surface.
    ///
    /// # Errors
    ///
    /// [`RenderError::NoAdapter`] if nothing suitable exists,
    /// [`RenderError::NoDevice`] if one exists and refuses.
    pub fn open(
        instance: &wgpu::Instance,
        surface: Option<&wgpu::Surface<'_>>,
    ) -> Result<Self, RenderError> {
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            // Charter rule 18: the client targets a modest discrete card, and
            // integrated graphics is best-effort. Where there is a choice, take
            // the faster one.
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: surface,
        }))
        .map_err(|err| RenderError::NoAdapter(err.to_string()))?;

        let info = adapter.get_info();
        // Asked for, not required. A device without it falls back to textured
        // rendering with a warning rather than failing to start — wireframe is
        // a diagnostic, not a feature anyone plays with.
        let wireframe = adapter
            .features()
            .contains(wgpu::Features::POLYGON_MODE_LINE);

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("tiamot-client"),
            required_features: if wireframe {
                wgpu::Features::POLYGON_MODE_LINE
            } else {
                wgpu::Features::empty()
            },
            // The adapter's own limits rather than the defaults: the defaults
            // are the downlevel web ones, and a chunk's index buffer exceeds
            // some of them on a world of any size.
            required_limits: adapter.limits(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
        }))
        .map_err(|err| RenderError::NoDevice {
            adapter: info.name.clone(),
            reason: err.to_string(),
        })?;

        Ok(Self {
            device,
            queue,
            adapter: info.name,
            backend: format!("{:?}", info.backend),
            polygon_mode_line: wireframe,
        })
    }

    /// Creates a device with no surface, for offscreen rendering.
    ///
    /// # Errors
    ///
    /// As [`Gpu::open`].
    pub fn headless() -> Result<Self, RenderError> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        Self::open(&instance, None)
    }
}

/// Draws the world.
pub struct Renderer {
    gpu: Gpu,
    pipeline: wgpu::RenderPipeline,
    globals: wgpu::Buffer,
    bind_layout: wgpu::BindGroupLayout,
    bind_group: wgpu::BindGroup,
    sampler: wgpu::Sampler,
    /// The atlas geometry the shader needs, mirroring what was uploaded.
    atlas_grid: u32,
    atlas_side: u32,
    chunks: BTreeMap<ChunkPos, ChunkMesh>,
    instances: wgpu::Buffer,
    instance_capacity: usize,
    depth: wgpu::TextureView,
    depth_size: (u32, u32),
    mode: RenderMode,
    /// How many chunks the last frame actually drew.
    drawn: usize,
}

impl Renderer {
    /// Builds the pipeline and a placeholder atlas.
    ///
    /// The placeholder is a single magenta checker, so a client that draws
    /// before the material table arrives shows "no textures yet" rather than
    /// sampling an unbound texture — which is a validation error on some
    /// backends and undefined colours on others.
    ///
    /// # Errors
    ///
    /// Currently infallible; the signature leaves room for shader compilation
    /// to be reported rather than panicked on.
    pub fn new(gpu: Gpu, mode: RenderMode, width: u32, height: u32) -> Result<Self, RenderError> {
        let shader = gpu
            .device
            .create_shader_module(wgpu::include_wgsl!("world.wgsl"));

        let bind_layout = gpu
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("world-bind-layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
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
                ],
            });

        let pipeline = build_pipeline(&gpu, &shader, &bind_layout, mode);

        let globals = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("globals"),
            size: size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let sampler = gpu.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("atlas"),
            // Repeat, because the shader's `fract` already wraps — this only
            // matters for the derivative-driven mip levels at grazing angles.
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            address_mode_w: wgpu::AddressMode::Repeat,
            // NEAREST magnification: voxel textures are pixel art, and
            // smoothing them is what turns a 16-pixel tile into a smear.
            // Linear minification and mip interpolation, because the
            // alternative is aliasing that shimmers as the camera moves.
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });

        let placeholder = Atlas::build(&[None]);
        let (view, grid, side) = upload_atlas(&gpu, &placeholder);
        let bind_group = make_bind_group(&gpu, &bind_layout, &globals, &view, &sampler);

        let instances = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("chunk-instances"),
            size: (size_of::<Instance>() * 64) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let depth = make_depth(&gpu, width, height);

        Ok(Self {
            gpu,
            pipeline,
            globals,
            bind_layout,
            bind_group,
            sampler,
            atlas_grid: grid,
            atlas_side: side,
            chunks: BTreeMap::new(),
            instances,
            instance_capacity: 64,
            depth,
            depth_size: (width, height),
            mode,
            drawn: 0,
        })
    }

    /// The device this renderer draws with.
    #[must_use]
    pub const fn gpu(&self) -> &Gpu {
        &self.gpu
    }

    /// How many chunk meshes are resident.
    #[must_use]
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// How many chunks the last frame drew, after culling.
    #[must_use]
    pub const fn drawn(&self) -> usize {
        self.drawn
    }

    /// Replaces the atlas.
    ///
    /// Called once, when the material table and its textures arrive. Rebuilds
    /// the bind group because the texture view it referenced is gone.
    pub fn set_atlas(&mut self, atlas: &Atlas) {
        let (view, grid, side) = upload_atlas(&self.gpu, atlas);
        self.atlas_grid = grid;
        self.atlas_side = side;
        self.bind_group = make_bind_group(
            &self.gpu,
            &self.bind_layout,
            &self.globals,
            &view,
            &self.sampler,
        );
    }

    /// Uploads a chunk's mesh, replacing any previous one.
    ///
    /// An empty mesh **removes** the chunk rather than storing a zero-length
    /// buffer: a chunk that was solid and has been dug out entirely must stop
    /// being drawn, and a zero-index draw call is a per-frame cost for nothing.
    pub fn set_chunk(&mut self, pos: ChunkPos, mesh: &Mesh) {
        if mesh.is_empty() {
            self.chunks.remove(&pos);
            return;
        }

        let (vertices, indices) = mesh.to_buffers();
        let vertex_buffer = self
            .gpu
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("chunk-vertices"),
                contents: bytemuck::cast_slice(&vertices),
                usage: wgpu::BufferUsages::VERTEX,
            });
        let index_buffer = self
            .gpu
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("chunk-indices"),
                contents: bytemuck::cast_slice(&indices),
                usage: wgpu::BufferUsages::INDEX,
            });

        self.chunks.insert(
            pos,
            ChunkMesh {
                vertices: vertex_buffer,
                indices: index_buffer,
                index_count: u32::try_from(indices.len()).unwrap_or(0),
            },
        );
    }

    /// Drops a chunk's mesh.
    pub fn remove_chunk(&mut self, pos: ChunkPos) {
        self.chunks.remove(&pos);
    }

    /// Forgets every mesh, for a reconnection.
    pub fn clear(&mut self) {
        self.chunks.clear();
    }

    /// Moves every resident mesh by a whole number of chunks.
    ///
    /// No GPU work: a mesh's vertices are in chunk-local sub-node units, so
    /// where a chunk *is* lives entirely in its key and moving the world is
    /// re-keying a map. Used by the floating-origin debug teleport to carry
    /// the world along with the camera — geometry left behind at the origin
    /// while the camera jumps 50,000 blocks is simply beyond the far plane,
    /// which shows an empty sky rather than the artefact being looked for.
    pub fn rebase(&mut self, delta: [i32; 3]) {
        if delta == [0, 0, 0] {
            return;
        }
        self.chunks = std::mem::take(&mut self.chunks)
            .into_iter()
            .map(|(pos, mesh)| {
                (
                    ChunkPos::new(pos.x + delta[0], pos.y + delta[1], pos.z + delta[2]),
                    mesh,
                )
            })
            .collect();
    }

    /// Total VRAM the resident chunk meshes occupy, as the device reports it.
    ///
    /// Read back from the buffers rather than computed from the mesh sizes:
    /// wgpu rounds every allocation up, and the rounding is what the VRAM bound
    /// in the Task 02b verdict is measured against.
    #[must_use]
    pub fn mesh_bytes(&self) -> u64 {
        self.chunks
            .values()
            .map(|chunk| chunk.vertices.size() + chunk.indices.size())
            .sum()
    }

    /// Renders one frame into `target`.
    ///
    /// `size` is the target's dimensions; the depth buffer is rebuilt when they
    /// change, because a depth attachment must match its colour attachment
    /// exactly or the pass fails to begin.
    pub fn render(&mut self, target: &wgpu::TextureView, camera: &Camera, size: (u32, u32)) {
        if size != self.depth_size && size.0 > 0 && size.1 > 0 {
            self.depth = make_depth(&self.gpu, size.0, size.1);
            self.depth_size = size;
        }

        let aspect = size.0 as f32 / size.1.max(1) as f32;
        let view_projection = camera.view_projection(aspect);

        self.gpu.queue.write_buffer(
            &self.globals,
            0,
            bytemuck::bytes_of(&Globals {
                view_projection: view_projection.to_cols_array_2d(),
                atlas_grid: self.atlas_grid,
                atlas_side: self.atlas_side,
                tile: crate::texture::TILE,
                padding: crate::texture::PADDING,
                render_mode: u32::from(self.mode == RenderMode::Flat),
                pad: [0; 3],
            }),
        );

        // Cull, then build the instance array. Both in one pass so a chunk's
        // offset is computed exactly once per frame.
        let frustum = Frustum::from_view_projection(view_projection);
        let mut visible = Vec::with_capacity(self.chunks.len());
        let mut instances = Vec::with_capacity(self.chunks.len());
        for (pos, mesh) in &self.chunks {
            let offset = camera.position.chunk_offset(*pos);
            if !frustum.contains_chunk(offset) {
                continue;
            }
            visible.push(mesh);
            instances.push(Instance {
                offset: [offset.x, offset.y, offset.z, 0.0],
            });
        }
        self.drawn = visible.len();

        if instances.len() > self.instance_capacity {
            // Grown in powers of two rather than to the exact size, so a world
            // filling in does not reallocate on almost every frame.
            let capacity = instances.len().next_power_of_two();
            self.instances = self.gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("chunk-instances"),
                size: (size_of::<Instance>() * capacity) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.instance_capacity = capacity;
        }
        if !instances.is_empty() {
            self.gpu
                .queue
                .write_buffer(&self.instances, 0, bytemuck::cast_slice(&instances));
        }

        let mut encoder = self
            .gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame"),
            });

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("world"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(SKY),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.set_vertex_buffer(1, self.instances.slice(..));

            for (index, mesh) in visible.iter().enumerate() {
                pass.set_vertex_buffer(0, mesh.vertices.slice(..));
                pass.set_index_buffer(mesh.indices.slice(..), wgpu::IndexFormat::Uint32);
                // The instance range picks this chunk's offset out of the
                // shared array — one buffer write per frame instead of one per
                // chunk.
                let instance = index as u32;
                pass.draw_indexed(0..mesh.index_count, 0, instance..instance + 1);
            }
        }

        self.gpu.queue.submit(Some(encoder.finish()));
    }
}

/// Builds the world pipeline.
///
/// A function rather than more of [`Renderer::new`], which was long enough that
/// the interesting decisions in it — winding, culling, depth comparison —
/// were buried in the middle of resource creation.
fn build_pipeline(
    gpu: &Gpu,
    shader: &wgpu::ShaderModule,
    bind_layout: &wgpu::BindGroupLayout,
    mode: RenderMode,
) -> wgpu::RenderPipeline {
    let layout = gpu
        .device
        .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("world-pipeline-layout"),
            bind_group_layouts: &[Some(bind_layout)],
            immediate_size: 0,
        });

    let wireframe = mode == RenderMode::Wireframe && gpu.polygon_mode_line;
    if mode == RenderMode::Wireframe && !wireframe {
        tracing::warn!(
            adapter = %gpu.adapter,
            "this adapter cannot draw in wireframe (no POLYGON_MODE_LINE); falling back to \
             textured"
        );
    }

    gpu.device
        .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("world"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: shader,
                entry_point: Some("vertex_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[
                    wgpu::VertexBufferLayout {
                        array_stride: size_of::<PackedVertex>() as u64,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &[
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Uint32,
                                offset: 0,
                                shader_location: 0,
                            },
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Uint32,
                                offset: 4,
                                shader_location: 1,
                            },
                        ],
                    },
                    wgpu::VertexBufferLayout {
                        array_stride: size_of::<Instance>() as u64,
                        step_mode: wgpu::VertexStepMode::Instance,
                        attributes: &[wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x4,
                            offset: 0,
                            shader_location: 2,
                        }],
                    },
                ],
            },
            fragment: Some(wgpu::FragmentState {
                module: shader,
                entry_point: Some("fragment_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: COLOUR_FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                // The mesher winds both face directions counter-clockwise when
                // seen from outside, so back-face culling halves the triangles
                // rasterised. Getting the winding wrong makes exactly half the
                // world invisible, which looks like a meshing bug rather than a
                // pipeline one.
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

/// Uploads an atlas and its mip chain, returning the view and its geometry.
fn upload_atlas(gpu: &Gpu, atlas: &Atlas) -> (wgpu::TextureView, u32, u32) {
    let levels = atlas.mips();
    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("atlas"),
        size: wgpu::Extent3d {
            width: atlas.side(),
            height: atlas.side(),
            depth_or_array_layers: 1,
        },
        mip_level_count: levels.len() as u32,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: COLOUR_FORMAT,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });

    for (level, image) in levels.iter().enumerate() {
        gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: level as u32,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &image.rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(image.width * 4),
                rows_per_image: Some(image.height),
            },
            wgpu::Extent3d {
                width: image.width,
                height: image.height,
                depth_or_array_layers: 1,
            },
        );
    }

    (
        texture.create_view(&wgpu::TextureViewDescriptor::default()),
        atlas.grid,
        atlas.side(),
    )
}

fn make_bind_group(
    gpu: &Gpu,
    layout: &wgpu::BindGroupLayout,
    globals: &wgpu::Buffer,
    atlas: &wgpu::TextureView,
    sampler: &wgpu::Sampler,
) -> wgpu::BindGroup {
    gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("world"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: globals.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(atlas),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
        ],
    })
}

fn make_depth(gpu: &Gpu, width: u32, height: u32) -> wgpu::TextureView {
    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("depth"),
        size: wgpu::Extent3d {
            width: width.max(1),
            height: height.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: DEPTH_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    texture.create_view(&wgpu::TextureViewDescriptor::default())
}
