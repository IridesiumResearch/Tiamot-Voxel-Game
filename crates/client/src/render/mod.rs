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
pub mod graph;
pub mod offscreen;
pub mod shadow;

use std::collections::BTreeMap;

use tiamot_core::ChunkPos;

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
    /// Task 10's lighting mode: 0 simple, 1 classic, 2 beautiful.
    ///
    /// Separate from `render_mode`, which says what *surface data* to draw
    /// (atlas, flat, wireframe) and is a debugging aid. This says how what is
    /// drawn is lit, and is a player-facing setting.
    lighting_mode: u32,
    /// How bright the sun is, `0.0..=1.0`.
    ///
    /// **The stored sunlight channel is always full daylight**, and this scales
    /// it at draw time. That is what lets dusk cost nothing: a value that meant
    /// "the current sun" would dirty every chunk in the world twenty times a
    /// second.
    sun_intensity: f32,
    /// The floor under the darkest place, so a cave is legible rather than
    /// pitch black. Presentation only — the stored light really is zero there.
    ambient: f32,
    /// Where fog starts, in blocks from the camera.
    fog_start: f32,
    /// Padding to the 16-byte boundary the `vec4`s below sit on.
    ///
    /// Not decorative. WGSL aligns a `vec4<f32>` to 16 bytes, so the shader
    /// reads `sun_colour` from offset 112 whatever this side does; without the
    /// three words the Rust struct ends at 108 and every colour arrives shifted.
    _pad: [u32; 3],
    /// The sun's colour, which a mod sets through the sky (Task 10).
    sun_colour: [f32; 4],
    /// The sky's colour in `xyz`, and where fog reaches full strength in `w`.
    ///
    /// **Fog is the sky's colour or it does not work.** Fading geometry towards
    /// any other colour puts a coloured haze between the player and the
    /// horizon; fading it towards the sky makes distant terrain dissolve into
    /// the sky it is standing against, which is what hides the edge of the
    /// loaded world.
    sky_colour: [f32; 4],
    /// One world-to-light matrix per shadow cascade. Written in every mode and
    /// read only by mode 3 — 192 bytes in a uniform that is rewritten once a
    /// frame, against a second buffer and a second write to avoid it.
    ///
    /// **These come after `sky_colour` because that is where `world.wgsl` has
    /// them.** The two declarations are one memory layout written down twice
    /// and nothing checks that they agree: putting these fields before the sky
    /// colour made the shader read a matrix row as the fog colour, and the
    /// distant world came out pure red. `distant_terrain_fades_into_the_sky`
    /// is what caught it, within one test run of the mistake.
    light_view_projection: [[[f32; 4]; 4]; shadow::CASCADES],
    /// Where each cascade ends, in blocks, and one shadow texel in `w`.
    cascade_far: [f32; 4],
    /// The direction the sun's light travels, and a spare word.
    ///
    /// The world shader needs it to ask whether a face points at the sun at
    /// all, which is a question no depth map can answer — see `shadow_factor`.
    /// **Appended rather than slotted in beside the other sun fields**, for the
    /// reason `light_view_projection` documents above: every field after an
    /// insertion moves, and the shader finds out by reading the wrong sixteen
    /// bytes.
    sun_direction: [f32; 4],
    /// The world size of one shadow texel, in blocks, per cascade.
    ///
    /// The normal-offset bias is measured in these. Computed here rather than
    /// in the shader because the cascade radius lives on this side and the
    /// alternative is recovering it from the length of a matrix row.
    shadow_texel: [f32; 4],
}

/// How much light the darkest place still gets.
///
/// A pure black cave is not atmospheric, it is unplayable — a player cannot see
/// the wall they are standing against. This is presentation only: the stored
/// light really is zero down there, and `game.get_light` tells a mod so.
const AMBIENT_FLOOR: f32 = 0.03;

/// How far out fog begins, as a fraction of where it becomes total.
///
/// Three quarters leaves a band deep enough to hide a chunk arriving without
/// washing out the middle distance a player is actually looking at.
const FOG_START_FRACTION: f32 = 0.75;

/// The default sky, as the shader wants it.
///
/// The same value the frame is cleared to. Fog and background must agree or
/// the horizon has a seam exactly where the fog was supposed to hide one.
#[must_use]
pub fn sky_colour() -> [f32; 3] {
    [SKY.r as f32, SKY.g as f32, SKY.b as f32]
}

/// Uploads a mesh into its own pair of buffers.
///
/// For geometry that is not a chunk and never changes — the debug body — so it
/// does not go through the chunk pool, which exists to recycle buffers for
/// meshes that are rebuilt constantly.
fn upload_mesh(gpu: &Gpu, mesh: &Mesh) -> ChunkMesh {
    let (vertices, indices) = mesh.to_buffers();
    let vertex_bytes: &[u8] = bytemuck::cast_slice(&vertices);
    let index_bytes: &[u8] = bytemuck::cast_slice(&indices);

    let vertex_buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("body-vertices"),
        size: vertex_bytes.len() as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let index_buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("body-indices"),
        size: index_bytes.len() as u64,
        usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    gpu.queue.write_buffer(&vertex_buffer, 0, vertex_bytes);
    gpu.queue.write_buffer(&index_buffer, 0, index_bytes);

    ChunkMesh {
        vertices: vertex_buffer,
        indices: index_buffer,
        index_count: u32::try_from(indices.len()).unwrap_or(0),
        used_bytes: (vertex_bytes.len() + index_bytes.len()) as u64,
    }
}

/// How wide the debug body is, in cells.
///
/// Rounded from the real AABB: 1.8 cells wide becomes 2, because a quad's
/// extents are whole cells. A tenth of a block wider than the body it stands
/// in, which nothing depends on — it is a shadow caster, not a hitbox.
pub const BODY_WIDTH_CELLS: u8 = 2;

/// A box the size of a player, for third-person view and for casting a shadow.
///
/// # Why the engine draws this at all
///
/// Entities are Task 12 and there is no player model. But a world with nothing
/// in it that MOVES has no moving shadow, and "do the cascades look right" is a
/// question you cannot answer by looking at a static pillar — the artefacts
/// that matter (edges crawling, the near cascade's bias, a caster leaving its
/// own shadow behind) all need something walking about.
///
/// So: a box, in the shape of the collision AABB, drawn only in third person.
/// It is not a placeholder for a player model and should not grow into one;
/// when Task 12 brings real entities this goes.
///
/// # Why it is built from quads rather than meshed
///
/// The mesher's job is a chunk of voxels. This is six faces at known positions,
/// and running it through a mesher would mean inventing a voxel grid to hold
/// something already known.
fn body_mesh() -> Mesh {
    use crate::mesher::Quad;
    use crate::shade::Shade;

    const WIDTH: u8 = BODY_WIDTH_CELLS;
    const HEIGHT: u8 = 5;
    /// The atlas slot it is drawn with. Slot 2 is the first real material in
    /// every world this ships with, and a body that used the placeholder would
    /// be magenta.
    const MATERIAL: u16 = 2;

    let lit = Shade {
        light: [tiamot_core::light::Light::DAYLIGHT; 4],
        // Full daylight lands exactly on a level, so there is no quarter of one
        // left over.
        fine: [0; 4],
        // Fully open: a floating box has nothing boxing it in, and giving it
        // occlusion would darken its corners for no geometric reason.
        occlusion: [3; 4],
    };
    let mut quads = Vec::with_capacity(6);
    // A positive face sits at `w + 1` — see `Mesh::to_buffers` — so the far
    // side of a box spanning cells `0..n` is quad `w = n - 1`, not `n`. Writing
    // the extent there stretches the box a cell past itself on three sides.
    for (axis, positive, w, du, dv) in [
        (0u8, false, 0, HEIGHT, WIDTH),
        (0, true, WIDTH - 1, HEIGHT, WIDTH),
        (1, false, 0, WIDTH, WIDTH),
        (1, true, HEIGHT - 1, WIDTH, WIDTH),
        (2, false, 0, WIDTH, HEIGHT),
        (2, true, WIDTH - 1, WIDTH, HEIGHT),
    ] {
        quads.push(Quad {
            axis,
            positive,
            w,
            u: 0,
            v: 0,
            du,
            dv,
            material: MATERIAL,
            shade: lit,
        });
    }
    Mesh { quads }
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
    /// Bytes actually written, as opposed to the pooled buffers' capacity.
    used_bytes: u64,
}

/// Line-segment vertices the selection buffer holds without growing.
///
/// A box is 12 segments, so this is 32 boxes — comfortably more than the 27
/// cells of a single block, which is the largest thing any brush can outline
/// today. Sized once rather than grown, because a buffer that reallocates every
/// time the crosshair moves is the churn `BufferPool` exists to avoid.
const SELECTION_CAPACITY: usize = 32 * 12 * 2;

/// The smallest buffer the pool hands out, in bytes.
///
/// Chunk meshes in open terrain are startlingly small — a flat surface greedily
/// merges to a handful of quads, so a few hundred bytes is typical. Rounding
/// those up to 4 KiB costs a rounding error of VRAM and collapses almost every
/// chunk into one size class, which is what makes the pool hit.
const MIN_BUFFER_BYTES: u64 = 4 * 1024;

/// How much VRAM the pool may hold in buffers nothing is using, in bytes.
///
/// Without a cap this is a memory leak with extra steps: fly through a cave
/// system, retire a few thousand large meshes, and every one of those buffers
/// is held for ever against a size class that may never be asked for again.
const POOL_CAPACITY_BYTES: u64 = 64 * 1024 * 1024;

/// Retired buffers, kept for reuse rather than freed.
///
/// **This exists because buffer creation is what makes chunk streaming hitch,
/// and it costs the same whether the buffer is large or tiny.** Measured on an
/// RTX 5070 Ti, rebuilding four chunks cost 15.6 ms of a 17.0 ms frame while
/// the meshes involved totalled well under a megabyte — a cost that ignores
/// data size is allocation overhead, not bandwidth. `create_buffer_init` was
/// allocating two fresh device buffers per chunk and freeing two more, eight
/// allocator operations per frame, and driver allocators are not built to be
/// called at that rate.
///
/// Walking gives the pool its hit rate for free: the interest volume is a
/// roughly constant size, so a chunk arriving means a chunk leaving, and the
/// buffers the departing one gives back are the right size class for the
/// arrival. Steady-state creation drops to near zero.
///
/// **This cannot be measured on a software rasteriser** — under lavapipe the
/// whole thing is a `malloc` and the difference is invisible. What IS
/// driver-independent, and what the tests assert, is the number of buffers
/// created: that count is the mechanism, so pinning it pins the fix.
#[derive(Default)]
struct BufferPool {
    /// Free vertex buffers by capacity, largest class last.
    vertices: BTreeMap<u64, Vec<wgpu::Buffer>>,
    /// Free index buffers by capacity.
    indices: BTreeMap<u64, Vec<wgpu::Buffer>>,
    /// Bytes currently held idle across both maps.
    idle_bytes: u64,
    /// Buffers created over this renderer's lifetime.
    created: u64,
    /// Requests served from the pool rather than the device.
    reused: u64,
}

/// Which of the two pools a buffer belongs to.
///
/// Vertex and index buffers are not interchangeable — a buffer's usage flags
/// are fixed when it is created, and handing an `INDEX` buffer back as a vertex
/// buffer is a validation error, not a slow path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BufferKind {
    Vertex,
    Index,
}

impl BufferKind {
    const fn usage(self) -> wgpu::BufferUsages {
        // `COPY_DST` is what makes reuse possible at all: a pooled buffer is
        // filled with `write_buffer` rather than at creation, and a buffer
        // without this flag can only ever be written once, when it is made.
        match self {
            Self::Vertex => wgpu::BufferUsages::VERTEX.union(wgpu::BufferUsages::COPY_DST),
            Self::Index => wgpu::BufferUsages::INDEX.union(wgpu::BufferUsages::COPY_DST),
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Vertex => "chunk-vertices",
            Self::Index => "chunk-indices",
        }
    }
}

impl BufferPool {
    /// The capacity class that holds `bytes`.
    ///
    /// Powers of two from [`MIN_BUFFER_BYTES`], so the number of classes stays
    /// small and a buffer freed by one chunk fits the next chunk of roughly the
    /// same size. Exact-fit classes would make almost every request a miss.
    fn class_for(bytes: u64) -> u64 {
        let mut class = MIN_BUFFER_BYTES;
        while class < bytes {
            class *= 2;
        }
        class
    }

    /// A buffer of at least `bytes`, from the pool if one fits.
    fn take(&mut self, gpu: &Gpu, kind: BufferKind, bytes: u64) -> wgpu::Buffer {
        let class = Self::class_for(bytes);
        let free = match kind {
            BufferKind::Vertex => &mut self.vertices,
            BufferKind::Index => &mut self.indices,
        };

        if let Some(bucket) = free.get_mut(&class)
            && let Some(buffer) = bucket.pop()
        {
            self.idle_bytes = self.idle_bytes.saturating_sub(class);
            self.reused += 1;
            return buffer;
        }

        self.created += 1;
        gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(kind.label()),
            size: class,
            usage: kind.usage(),
            mapped_at_creation: false,
        })
    }

    /// Takes a retired buffer back, or drops it if the pool is full.
    fn give(&mut self, kind: BufferKind, buffer: wgpu::Buffer) {
        let class = buffer.size();
        if self.idle_bytes + class > POOL_CAPACITY_BYTES {
            // Dropped rather than kept. Letting the pool grow without bound
            // would trade a frame-time problem for a VRAM one, and charter rule
            // 19 retired the inflation-ratio gate in favour of an absolute VRAM
            // bound precisely so this sort of thing stays bounded.
            return;
        }
        self.idle_bytes += class;
        let free = match kind {
            BufferKind::Vertex => &mut self.vertices,
            BufferKind::Index => &mut self.indices,
        };
        free.entry(class).or_default().push(buffer);
    }

    /// Takes both of a retired mesh's buffers back.
    fn give_mesh(&mut self, mesh: ChunkMesh) {
        self.give(BufferKind::Vertex, mesh.vertices);
        self.give(BufferKind::Index, mesh.indices);
    }
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
    /// Retired chunk buffers, kept for reuse. See [`BufferPool`].
    pool: BufferPool,
    instances: wgpu::Buffer,
    instance_capacity: usize,
    depth: wgpu::TextureView,
    depth_size: (u32, u32),
    mode: RenderMode,
    /// How sharp the cascades are, or whether there are any.
    shadow_quality: crate::config::ShadowQuality,
    /// Which of Task 10's lighting modes is showing.
    ///
    /// Changed live by [`Renderer::set_lighting_mode`]. The pipeline does not
    /// change with it: both modes are one shader and a uniform, because a mode
    /// that swapped pipelines could not be switched without a stall and would
    /// be two code paths to keep agreeing.
    lighting: crate::config::LightingMode,
    /// The world and selection shaders, kept so mode 3 can compile the same
    /// code against its float target rather than loading a second copy.
    world_shader: wgpu::ShaderModule,
    selection_shader: wgpu::ShaderModule,
    /// Mode 3's targets and post chain, built when that mode is showing and
    /// dropped when it is not.
    ///
    /// `None` is the criterion: "no shadow/post allocations when in mode 1" is
    /// a property something can assert, and [`Renderer::post_bytes`] is how.
    post: Option<graph::Post>,
    /// How bright the sun is now, `0.0..=1.0`.
    ///
    /// Set by the sky each frame once time of day exists; full daylight until
    /// then, which is what Task 08's scenes assumed.
    sun_intensity: f32,
    /// The sun's colour now.
    sun_colour: [f32; 4],
    /// Which way its light travels, for the cascades. Set by the sky with the
    /// colour, and pointing sensibly downward until one arrives.
    sun_direction: [f32; 3],
    /// The sky's colour now, which fog fades towards.
    sky_colour: [f32; 3],
    /// Where fog begins and where it is total, in blocks.
    fog_start: f32,
    fog_end: f32,
    /// How many chunks the last frame actually drew.
    drawn: usize,
    /// The debug body's mesh, built once, and where it is this frame.
    ///
    /// `None` in first person, which is every frame until somebody asks for
    /// third — a box drawn around the camera is a box drawn inside the player's
    /// head, and all you see is its inside faces.
    body: ChunkMesh,
    body_at: Option<[f32; 3]>,
    /// The outline pipeline, and the line segments it draws this frame.
    selection_pipeline: wgpu::RenderPipeline,
    selection: wgpu::Buffer,
    /// Vertices actually written, which is twice the segment count.
    selection_vertices: u32,
    /// How many vertices [`Renderer::selection`] can hold.
    selection_capacity: usize,
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

        let bind_layout = build_bind_layout(&gpu);

        let pipeline = build_pipeline(&gpu, &shader, &bind_layout, mode, COLOUR_FORMAT);

        let selection_shader = gpu
            .device
            .create_shader_module(wgpu::include_wgsl!("selection.wgsl"));
        let selection_pipeline =
            build_selection_pipeline(&gpu, &selection_shader, &bind_layout, COLOUR_FORMAT);
        let body_buffers = upload_mesh(&gpu, &body_mesh());

        let selection_buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("selection"),
            size: (SELECTION_CAPACITY * size_of::<[f32; 3]>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

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
            pool: BufferPool::default(),
            selection_pipeline,
            selection: selection_buffer,
            selection_vertices: 0,
            selection_capacity: SELECTION_CAPACITY,
            instances,
            instance_capacity: 64,
            depth,
            depth_size: (width, height),
            mode,
            // Classic until told otherwise: a client with no config gets the
            // mode the world was lit for.
            lighting: crate::config::LightingMode::default(),
            shadow_quality: crate::config::ShadowQuality::default(),
            world_shader: shader,
            selection_shader,
            post: None,
            // Full daylight until a sky says otherwise, which is what Task
            // 08's scenes assumed and what a world with no sky mod gets.
            sun_intensity: 1.0,
            sun_colour: [1.0, 1.0, 1.0, 1.0],
            sun_direction: [0.0, -0.970_142_5, 0.242_535_62],
            sky_colour: sky_colour(),
            // Far enough that nothing fogs until a view distance is set. A
            // client that fogged by default would hide geometry the Task 08
            // scenes assert on.
            fog_start: f32::MAX,
            fog_end: f32::MAX,
            drawn: 0,
            body: body_buffers,
            body_at: None,
        })
    }

    /// The device this renderer draws with.
    #[must_use]
    pub const fn gpu(&self) -> &Gpu {
        &self.gpu
    }

    /// Sets the sun's strength and colour for the frames that follow.
    ///
    /// **The world's stored sunlight never changes with the time of day** — it
    /// is always full daylight, and this scales it at draw time. Anything else
    /// would dirty every chunk in the world twenty times a second and relight
    /// them all for a change nobody can distinguish from a multiply.
    pub fn set_sun(&mut self, intensity: f32, colour: [f32; 3], direction: [f32; 3]) {
        self.sun_intensity = intensity.clamp(0.0, 1.0);
        self.sun_colour = [colour[0], colour[1], colour[2], 1.0];
        self.sun_direction = direction;
    }

    /// Sets the sky's colour and where fog fades to it.
    ///
    /// `far` is the distance at which terrain is entirely sky, in blocks —
    /// normally the view distance. **Fog starts well before it**, because fog
    /// that begins at the far plane has nothing to hide: chunks would appear
    /// at full contrast the instant they arrive, which is the pop the fog
    /// exists to cover.
    pub fn set_sky(&mut self, colour: [f32; 3], far: f32) {
        self.sky_colour = colour;
        self.fog_end = far.max(1.0);
        self.fog_start = self.fog_end * FOG_START_FRACTION;
    }

    /// The sun's current strength, for tests and the HUD.
    #[must_use]
    pub const fn sun_intensity(&self) -> f32 {
        self.sun_intensity
    }

    /// Puts the debug body somewhere, or takes it away.
    ///
    /// The offset is camera-relative, in blocks, like a chunk's — the whole
    /// renderer works that way (charter rule 7) and a body given a world
    /// position would be the one thing in the frame that did not.
    pub const fn set_body(&mut self, at: Option<[f32; 3]>) {
        self.body_at = at;
    }

    /// Switches the lighting mode.
    ///
    /// Takes effect on the next frame and allocates nothing: the mode is a
    /// uniform, so there is no pipeline to rebuild and no surface to
    /// reconfigure. The caller is responsible for remeshing — the mesher bakes
    /// light into vertices, so the geometry drawn under the new mode has to be
    /// rebuilt for it (see `App::set_lighting_mode`).
    pub fn set_lighting_mode(&mut self, mode: crate::config::LightingMode) {
        self.lighting = mode;
        if !mode.uses_post() {
            // Dropped rather than kept for later. A player who tried mode 3
            // once and went back to mode 1 for the frame rate should not still
            // be paying for its targets, and "mode 1 allocates none of this" is
            // only true if leaving gives it back.
            self.post = None;
        }
    }

    /// Which lighting mode is showing.
    #[must_use]
    pub const fn lighting_mode(&self) -> crate::config::LightingMode {
        self.lighting
    }

    /// Sets how sharp mode 3's shadows are.
    ///
    /// Takes effect on the next frame: the cascades are rebuilt because their
    /// resolution is the texture's, not a uniform.
    pub const fn set_shadow_quality(&mut self, quality: crate::config::ShadowQuality) {
        self.shadow_quality = quality;
    }

    /// How sharp mode 3's shadows are.
    #[must_use]
    pub const fn shadow_quality(&self) -> crate::config::ShadowQuality {
        self.shadow_quality
    }

    /// Texture memory the post chain is holding, in bytes. Zero unless mode 3
    /// is showing.
    #[must_use]
    pub fn post_bytes(&self) -> u64 {
        self.post.as_ref().map_or(0, graph::Post::bytes)
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
            self.remove_chunk(&pos);
            return;
        }

        let (vertices, indices) = mesh.to_buffers();
        let vertex_bytes: &[u8] = bytemuck::cast_slice(&vertices);
        let index_bytes: &[u8] = bytemuck::cast_slice(&indices);

        // The mesh being replaced gives its buffers back BEFORE the new ones
        // are asked for, so a remesh in place — the common case after a dig —
        // hands the same buffers straight back and allocates nothing at all.
        if let Some(previous) = self.chunks.remove(&pos) {
            self.pool.give_mesh(previous);
        }

        let vertex_buffer =
            self.pool
                .take(&self.gpu, BufferKind::Vertex, vertex_bytes.len() as u64);
        let index_buffer = self
            .pool
            .take(&self.gpu, BufferKind::Index, index_bytes.len() as u64);
        self.gpu.queue.write_buffer(&vertex_buffer, 0, vertex_bytes);
        self.gpu.queue.write_buffer(&index_buffer, 0, index_bytes);

        self.chunks.insert(
            pos,
            ChunkMesh {
                vertices: vertex_buffer,
                indices: index_buffer,
                index_count: u32::try_from(indices.len()).unwrap_or(0),
                used_bytes: (vertex_bytes.len() + index_bytes.len()) as u64,
            },
        );
    }

    /// Drops a chunk's mesh, returning its buffers to the pool.
    pub fn remove_chunk(&mut self, pos: &ChunkPos) {
        if let Some(mesh) = self.chunks.remove(pos) {
            self.pool.give_mesh(mesh);
        }
    }

    /// Forgets every mesh, for a reconnection.
    pub fn clear(&mut self) {
        for (_, mesh) in std::mem::take(&mut self.chunks) {
            self.pool.give_mesh(mesh);
        }
    }

    /// Sets the outline to draw, as camera-relative boxes in **cells**.
    ///
    /// Each entry is one cell's low corner. The caller has already applied the
    /// floating origin, exactly as it does for chunk instances — nothing in the
    /// render path ever sees a world coordinate (charter rule 7).
    ///
    /// Anything past [`SELECTION_CAPACITY`] is dropped rather than growing the
    /// buffer: the outline is a hint, and a hint is not worth a reallocation
    /// every time the crosshair moves.
    pub fn set_selection(&mut self, cells: &[[f32; 3]]) {
        let mut vertices: Vec<[f32; 3]> = Vec::with_capacity(cells.len() * 24);
        for cell in cells {
            push_box_edges(&mut vertices, *cell);
            if vertices.len() >= self.selection_capacity {
                vertices.truncate(self.selection_capacity);
                break;
            }
        }
        self.selection_vertices = u32::try_from(vertices.len()).unwrap_or(0);
        if !vertices.is_empty() {
            self.gpu
                .queue
                .write_buffer(&self.selection, 0, bytemuck::cast_slice(&vertices));
        }
    }

    /// How many GPU buffers this renderer has created, and how many requests
    /// the pool served instead.
    ///
    /// Exposed because creation count — not frame time — is the
    /// driver-independent measure of the streaming hitch. See [`BufferPool`].
    #[must_use]
    pub const fn buffer_stats(&self) -> (u64, u64) {
        (self.pool.created, self.pool.reused)
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
        // What was written, not what was allocated. Pooled buffers are rounded
        // up to a power-of-two class, so their `size()` would report the
        // rounding rather than the geometry and drift further from the truth
        // the better the pool works.
        self.chunks.values().map(|chunk| chunk.used_bytes).sum()
    }

    /// Everything the shader is told about this frame.
    ///
    /// Split out of [`Renderer::render`] because it is a list of settings and
    /// the draw loop is a sequence of steps; reading one while looking for the
    /// other is what makes a long function hard rather than its length.
    fn globals_for(&self, view_projection: glam::Mat4) -> Globals {
        Globals {
            view_projection: view_projection.to_cols_array_2d(),
            atlas_grid: self.atlas_grid,
            atlas_side: self.atlas_side,
            tile: crate::texture::TILE,
            padding: crate::texture::PADDING,
            render_mode: u32::from(self.mode == RenderMode::Flat),
            lighting_mode: self.lighting.code(),
            sun_intensity: self.sun_intensity,
            ambient: AMBIENT_FLOOR,
            fog_start: self.fog_start,
            _pad: [0; 3],
            sun_colour: self.sun_colour,
            // Fog's far distance rides in the sky colour's unused fourth
            // component rather than costing another sixteen bytes of padding.
            sky_colour: [
                self.sky_colour[0],
                self.sky_colour[1],
                self.sky_colour[2],
                self.fog_end,
            ],
            light_view_projection: self.post.as_ref().and_then(graph::Post::shadows).map_or(
                [glam::Mat4::IDENTITY.to_cols_array_2d(); shadow::CASCADES],
                |s| s.matrices().map(|m| m.to_cols_array_2d()),
            ),
            cascade_far: {
                let splits = shadow::Shadows::split_distances();
                let texels = self
                    .post
                    .as_ref()
                    .and_then(graph::Post::shadow_texels)
                    .unwrap_or(shadow::DEFAULT_SIZE);
                [splits[0], splits[1], splits[2], 1.0 / texels as f32]
            },
            sun_direction: [
                self.sun_direction[0],
                self.sun_direction[1],
                self.sun_direction[2],
                0.0,
            ],
            shadow_texel: {
                // A block per texel until a cascade has been fitted, which is
                // every frame in modes 1 and 2. Nothing reads it there.
                let world = self
                    .post
                    .as_ref()
                    .and_then(graph::Post::shadows)
                    .map_or([1.0; shadow::CASCADES], |s| *s.texel_world());
                [world[0], world[1], world[2], 0.0]
            },
        }
    }

    /// Frustum-culls the resident chunks and uploads their offsets.
    ///
    /// Returns the meshes to draw, in the order their instances were written —
    /// the draw loop indexes the instance array by position in this list, so
    /// the two must not be built separately.
    fn cull_and_upload(&mut self, camera: &Camera, view_projection: glam::Mat4) -> Vec<ChunkPos> {
        // Cull, then build the instance array. Both in one pass so a chunk's
        // offset is computed exactly once per frame.
        let frustum = Frustum::from_view_projection(view_projection);
        let mut visible = Vec::with_capacity(self.chunks.len());
        let mut instances = Vec::with_capacity(self.chunks.len());
        for pos in self.chunks.keys() {
            let offset = camera.position.chunk_offset(*pos);
            if !frustum.contains_chunk(offset) {
                continue;
            }
            // Positions rather than the meshes themselves: this method takes
            // `&mut self` to grow the instance buffer, and a borrow of a mesh
            // would keep that borrow alive across the draw loop. The lookup it
            // costs is one `BTreeMap` probe per drawn chunk per frame.
            visible.push(*pos);
            instances.push(Instance {
                offset: [offset.x, offset.y, offset.z, 0.0],
            });
        }
        self.drawn = visible.len();

        // The body rides at the end of the same array, so drawing it is one
        // more instance index rather than a second buffer and a second binding.
        if let Some(at) = self.body_at {
            instances.push(Instance {
                offset: [at[0], at[1], at[2], 0.0],
            });
        }

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

        visible
    }

    /// Builds or drops mode 3's targets to match the mode and the frame size.
    ///
    /// Checked every frame rather than on a mode change or a resize event, for
    /// the reason the depth buffer is: a target built for the old size draws
    /// the world into a texture the wrong shape, and a resize that arrives
    /// without an event — which happens — would leave it that way.
    fn prepare_post(&mut self, size: (u32, u32)) {
        if !self.lighting.uses_post() || size.0 == 0 || size.1 == 0 {
            self.post = None;
            return;
        }
        // Rebuilt when the frame changes size OR when the shadow setting does:
        // the cascades' resolution is baked into the textures, so a new quality
        // needs new ones.
        if self.post.as_ref().is_some_and(|post| {
            post.fits(size.0, size.1) && post.shadow_texels() == self.shadow_quality.texels()
        }) {
            return;
        }
        self.post = Some(graph::Post::new(
            &self.gpu,
            &graph::Shaders {
                world: &self.world_shader,
                selection: &self.selection_shader,
                layout: &self.bind_layout,
            },
            graph::Setup {
                mode: self.mode,
                width: size.0,
                height: size.1,
                shadow_texels: self.shadow_quality.texels(),
            },
        ));
    }

    /// Runs the post chain, if this mode has one.
    ///
    /// It reads the scene texture the world pass wrote and lands on `target`,
    /// so from outside the renderer a frame looks the same in every mode.
    fn run_post(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        view_projection: glam::Mat4,
    ) {
        let Some(post) = self.post.as_ref() else {
            return;
        };
        post.run(
            &self.gpu,
            encoder,
            target,
            &graph::Frame {
                inverse_view_projection: view_projection.inverse(),
                sky: self.sky_colour,
                sun: [self.sun_colour[0], self.sun_colour[1], self.sun_colour[2]],
                sun_direction: self.sun_direction,
                fog_start: self.fog_start,
                fog_end: self.fog_end,
            },
        );
    }

    /// Draws the visible chunks into every shadow cascade.
    ///
    /// The same meshes and the same instance buffer as the world pass, drawn
    /// from the sun instead of from the eye — which is why the shadow pipeline
    /// shares this module's vertex layout rather than having one of its own.
    /// Nothing happens in the modes with no cascades to fill.
    fn fill_cascades(&self, encoder: &mut wgpu::CommandEncoder, visible: &[ChunkPos]) {
        let Some(shadows) = self.post.as_ref().and_then(graph::Post::shadows) else {
            return;
        };
        shadows.render(encoder, |pass, _cascade| {
            pass.set_vertex_buffer(1, self.instances.slice(..));
            for (index, pos) in visible.iter().enumerate() {
                let Some(mesh) = self.chunks.get(pos) else {
                    continue;
                };
                pass.set_vertex_buffer(0, mesh.vertices.slice(..));
                pass.set_index_buffer(mesh.indices.slice(..), wgpu::IndexFormat::Uint32);
                let instance = index as u32;
                pass.draw_indexed(0..mesh.index_count, 0, instance..instance + 1);
            }
            // And the body, which is the only thing in the world that moves and
            // therefore the only way to see whether a moving shadow looks
            // right. Its instance is the one after the last chunk's.
            if self.body_at.is_some() {
                let instance = visible.len() as u32;
                pass.set_vertex_buffer(0, self.body.vertices.slice(..));
                pass.set_index_buffer(self.body.indices.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..self.body.index_count, 0, instance..instance + 1);
            }
        });
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

        self.prepare_post(size);

        let aspect = size.0 as f32 / size.1.max(1) as f32;
        let view_projection = camera.view_projection(aspect);

        // Before the globals are written, because the matrices go in them.
        if let Some(shadows) = self.post.as_mut().and_then(graph::Post::shadows_mut) {
            shadows.update(&self.gpu, camera, aspect, self.sun_direction);
        }

        self.gpu.queue.write_buffer(
            &self.globals,
            0,
            bytemuck::bytes_of(&self.globals_for(view_projection)),
        );

        let visible = self.cull_and_upload(camera, view_projection);

        let mut encoder = self
            .gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame"),
            });

        self.fill_cascades(&mut encoder, &visible);

        // Where the world goes: straight to the target in modes 1 and 2, and
        // into the float scene texture in mode 3 so the post chain has
        // something with headroom in it to read.
        let (colour, depth, world_pipeline, selection_pipeline) = match self.post.as_ref() {
            Some(post) => {
                let (scene, depth) = post.scene_target();
                (
                    scene,
                    depth,
                    post.world_pipeline(),
                    post.selection_pipeline(),
                )
            }
            None => (
                target,
                &self.depth,
                &self.pipeline,
                &self.selection_pipeline,
            ),
        };

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("world"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: colour,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(SKY),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: depth,
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

            pass.set_pipeline(world_pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            if let Some(shadows) = self.post.as_ref().and_then(graph::Post::shadows) {
                pass.set_bind_group(1, shadows.sample_bind(), &[]);
            }
            pass.set_vertex_buffer(1, self.instances.slice(..));

            for (index, pos) in visible.iter().enumerate() {
                let Some(mesh) = self.chunks.get(pos) else {
                    continue;
                };
                pass.set_vertex_buffer(0, mesh.vertices.slice(..));
                pass.set_index_buffer(mesh.indices.slice(..), wgpu::IndexFormat::Uint32);
                // The instance range picks this chunk's offset out of the
                // shared array — one buffer write per frame instead of one per
                // chunk.
                let instance = index as u32;
                pass.draw_indexed(0..mesh.index_count, 0, instance..instance + 1);
            }

            if self.body_at.is_some() {
                let instance = visible.len() as u32;
                pass.set_vertex_buffer(0, self.body.vertices.slice(..));
                pass.set_index_buffer(self.body.indices.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..self.body.index_count, 0, instance..instance + 1);
            }

            // Last, so it draws over the world it outlines. Its pipeline does
            // not write depth, so the order within the pass is what decides
            // this rather than the depth buffer.
            if self.selection_vertices > 0 {
                pass.set_pipeline(selection_pipeline);
                pass.set_vertex_buffer(0, self.selection.slice(..));
                pass.draw(0..self.selection_vertices, 0..1);
            }
        }

        // The post chain, if this mode has one. It reads the scene texture the
        // pass above just wrote and lands on `target`, so from outside the
        // renderer a frame looks the same in every mode.
        self.run_post(&mut encoder, target, view_projection);

        self.gpu.queue.submit(Some(encoder.finish()));
    }
}

/// Appends the twelve edges of a unit cube at `corner`, as line-list vertices.
///
/// Twelve segments, twenty-four vertices. A line *strip* would need degenerate
/// segments to jump between the disconnected edges of a cube, and those show up
/// as stray diagonals the moment anything upstream reorders them.
fn push_box_edges(out: &mut Vec<[f32; 3]>, corner: [f32; 3]) {
    // Bottom face, top face, then the four uprights joining them.
    const EDGES: [(usize, usize); 12] = [
        (0, 1),
        (1, 2),
        (2, 3),
        (3, 0),
        (4, 5),
        (5, 6),
        (6, 7),
        (7, 4),
        (0, 4),
        (1, 5),
        (2, 6),
        (3, 7),
    ];

    let [x, y, z] = corner;
    let corners = [
        [x, y, z],
        [x + 1.0, y, z],
        [x + 1.0, y, z + 1.0],
        [x, y, z + 1.0],
        [x, y + 1.0, z],
        [x + 1.0, y + 1.0, z],
        [x + 1.0, y + 1.0, z + 1.0],
        [x, y + 1.0, z + 1.0],
    ];
    for (from, to) in EDGES {
        out.push(corners[from]);
        out.push(corners[to]);
    }
}

/// Builds the selection-outline pipeline.
///
/// A line list with **no depth write and a `LessEqual` test**, which is the
/// combination that makes an outline usable. Writing depth would let the lines
/// occlude each other where they cross; a strict `Less` test would z-fight with
/// the very surface being outlined, since the outline sits exactly on it. This
/// draws on top of what it surrounds and leaves the buffer alone.
fn build_selection_pipeline(
    gpu: &Gpu,
    shader: &wgpu::ShaderModule,
    bind_layout: &wgpu::BindGroupLayout,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    let layout = gpu
        .device
        .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("selection-pipeline-layout"),
            bind_group_layouts: &[Some(bind_layout)],
            immediate_size: 0,
        });

    gpu.device
        .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("selection"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: shader,
                entry_point: Some("vertex_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: size_of::<[f32; 3]>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &[wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x3,
                        offset: 0,
                        shader_location: 0,
                    }],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: shader,
                entry_point: Some("fragment_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::LineList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(false),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        })
}

/// The one bind group every pipeline here shares: globals, atlas, sampler.
///
/// Extracted from [`Renderer::new`] because it is the same three entries for
/// the world pass and the selection pass, and because a constructor is easier
/// to read when the descriptor soup is somewhere else.
fn build_bind_layout(gpu: &Gpu) -> wgpu::BindGroupLayout {
    gpu.device
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
        })
}

/// The vertex and instance buffers a chunk mesh is drawn from.
///
/// Shared by the world pipeline and the shadow pipeline, which is the point:
/// both draw the same uploaded buffers, and a layout that had drifted between
/// them would put shadows somewhere the geometry is not.
///
/// Returned by value rather than as a constant because a `VertexBufferLayout`
/// borrows its attribute slice, and a `const` one cannot be referenced from two
/// pipelines without naming the lifetime everywhere.
fn vertex_layout() -> [wgpu::VertexBufferLayout<'static>; 2] {
    const VERTEX_ATTRIBUTES: [wgpu::VertexAttribute; 2] = [
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
    ];
    const INSTANCE_ATTRIBUTES: [wgpu::VertexAttribute; 1] = [wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x4,
        offset: 0,
        shader_location: 2,
    }];

    [
        wgpu::VertexBufferLayout {
            array_stride: size_of::<PackedVertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &VERTEX_ATTRIBUTES,
        },
        wgpu::VertexBufferLayout {
            array_stride: size_of::<Instance>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &INSTANCE_ATTRIBUTES,
        },
    ]
}

/// The world pipeline with the shadow cascades bound as a second group.
///
/// A separate function and a separate entry point rather than a flag, because
/// a bind group is part of a pipeline's layout: a single pipeline that could
/// optionally read shadows would need the maps to exist in every mode, which is
/// exactly the allocation Task 10 says mode 1 must not make.
fn build_shadowed_pipeline(
    gpu: &Gpu,
    shader: &wgpu::ShaderModule,
    bind_layout: &wgpu::BindGroupLayout,
    shadow_layout: &wgpu::BindGroupLayout,
    mode: RenderMode,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    build_world_pipeline(
        gpu,
        shader,
        &[Some(bind_layout), Some(shadow_layout)],
        "fragment_shadowed",
        mode,
        format,
    )
}

/// Builds the world pipeline.
///
/// A function rather than more of [`Renderer::new`], which was long enough that
/// the interesting decisions in it — winding, culling, depth comparison — were
/// buried in the middle of resource creation.
/// `format` is the colour target's, because a pipeline is compiled against one
/// and lighting mode 3 draws the world into a float texture rather than the
/// swapchain.
fn build_pipeline(
    gpu: &Gpu,
    shader: &wgpu::ShaderModule,
    bind_layout: &wgpu::BindGroupLayout,
    mode: RenderMode,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    build_world_pipeline(
        gpu,
        shader,
        &[Some(bind_layout)],
        "fragment_main",
        mode,
        format,
    )
}

/// The world pipeline, whichever bind groups and fragment stage it wants.
fn build_world_pipeline(
    gpu: &Gpu,
    shader: &wgpu::ShaderModule,
    bind_layouts: &[Option<&wgpu::BindGroupLayout>],
    fragment_entry: &str,
    mode: RenderMode,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    let layout = gpu
        .device
        .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("world-pipeline-layout"),
            bind_group_layouts: bind_layouts,
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
                buffers: &vertex_layout(),
            },
            fragment: Some(wgpu::FragmentState {
                module: shader,
                entry_point: Some(fragment_entry),
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
    make_depth_with(gpu, width, height, wgpu::TextureUsages::RENDER_ATTACHMENT)
}

/// A depth buffer that something else will read.
///
/// Mode 3's fog reconstructs where a surface is from its depth, so its depth
/// buffer is a texture binding as well as an attachment. The direct path's is
/// not — an attachment-only texture can live in memory a sampler cannot reach,
/// and there is no reason to give that up in the modes that never sample it.
fn make_sampled_depth(gpu: &Gpu, width: u32, height: u32) -> wgpu::TextureView {
    make_depth_with(
        gpu,
        width,
        height,
        wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
    )
}

fn make_depth_with(
    gpu: &Gpu,
    width: u32,
    height: u32,
    usage: wgpu::TextureUsages,
) -> wgpu::TextureView {
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
        usage,
        view_formats: &[],
    });
    texture.create_view(&wgpu::TextureViewDescriptor::default())
}
