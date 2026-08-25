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
pub mod grade;
pub mod graph;
pub mod offscreen;
pub mod shadow;
pub mod skinned;
pub mod viewmodel;

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
    /// Fluid's own word: seconds since the client started in `x`, three spare.
    ///
    /// **Appended**, for the reason `light_view_projection` documents: every
    /// field after an insertion moves, and the shader finds out by reading the
    /// wrong sixteen bytes.
    ///
    /// The clock is what makes milk move. It is presentation and nothing else
    /// reads it — charter rule 4 does not reach the scroll rate of a texture,
    /// and this value is deliberately not the simulation's tick.
    fluid: [f32; 4],
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
/// How wide the debug body is, in cells.
///
/// Rounded from the real AABB: 1.8 cells wide becomes 2, because a quad's
/// extents are whole cells. A tenth of a block wider than the body it stands
/// in, which nothing depends on — it is a shadow caster, not a hitbox.
pub const BODY_WIDTH_CELLS: u8 = 2;

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
    /// The fluid half, drawn in a second blended pass after every chunk's
    /// opaque geometry. `None` for the overwhelming majority of chunks, which
    /// have no milk in them and pay nothing for this.
    fluid: Option<FluidMesh>,
    /// Bytes actually written, as opposed to the pooled buffers' capacity.
    used_bytes: u64,
}

/// One chunk's transparent fluid geometry.
struct FluidMesh {
    vertices: wgpu::Buffer,
    indices: wgpu::Buffer,
    index_count: u32,
}

/// Line-segment vertices the selection buffer holds without growing.
///
/// A box is 12 segments, so this is 32 boxes — comfortably more than the 27
/// cells of a single block, which is the largest thing any brush can outline
/// today. Sized once rather than grown, because a buffer that reallocates every
/// time the crosshair moves is the churn `BufferPool` exists to avoid.
const SELECTION_CAPACITY: usize = 32 * 12 * 2;

/// Line-segment vertices the chunk-border buffer holds without growing.
///
/// A box is 12 segments, so this is 512 chunks — more than a default view
/// distance puts in front of the camera at once, and the overflow is dropped
/// rather than grown for the same reason the selection's is: a debug overlay is
/// not worth a reallocation on a frame where the view happens to be wide.
const CHUNK_BORDER_CAPACITY: usize = 512 * 12 * 2;

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
        if let Some(fluid) = mesh.fluid {
            self.give(BufferKind::Vertex, fluid.vertices);
            self.give(BufferKind::Index, fluid.indices);
        }
    }
}

/// The device, queue, and what we know about the adapter.
///
/// Separate from [`Renderer`] because the window needs an adapter before it can
/// choose a surface configuration, and a renderer that created its own device
/// would have to be built after the window rather than beside it.
///
/// **Cloning is cheap and does not make a second device.** `wgpu::Device` and
/// `wgpu::Queue` are handles to one thing; a clone is a second handle. The
/// window keeps one so it can configure its surface and draw a menu before
/// there is a renderer to ask.
#[derive(Clone)]
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

/// Where the fluid clock wraps, in seconds.
///
/// A large whole number of seconds, so a texture offset taken modulo one lands
/// in the same place either side of the wrap and nothing jumps.
const FLUID_CLOCK_WRAP: f32 = 3600.0;

/// Draws the world.
pub struct Renderer {
    gpu: Gpu,
    pipeline: wgpu::RenderPipeline,
    /// The blended pass that draws milk, over the direct target. Mode 3 uses
    /// the post chain's own copy, compiled for the float target instead.
    fluid_pipeline: wgpu::RenderPipeline,
    /// Seconds of animation, for the fluid scroll. See `advance_clock`.
    elapsed: f32,
    globals: wgpu::Buffer,
    bind_layout: wgpu::BindGroupLayout,
    bind_group: wgpu::BindGroup,
    sampler: wgpu::Sampler,
    /// The atlas geometry the shader needs, mirroring what was uploaded.
    atlas_grid: u32,
    atlas_side: u32,
    /// The atlas texture itself, kept so the interface can draw from it.
    ///
    /// **Kept only because egui needs a view to register.** The world pass
    /// reaches the atlas through `bind_group` and never touches this; a slot in
    /// an inventory has to draw the same pixels, and the alternative — a second
    /// upload of the same image for the UI — would double the atlas's memory
    /// just to show a player what they are carrying.
    atlas_view: wgpu::TextureView,
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
    /// The blob-shadow pipeline, and what it draws this frame.
    ///
    /// Compiled against the swapchain format; mode 3 draws the world into a
    /// float target and uses the second one.
    blob_pipeline: wgpu::RenderPipeline,
    /// The same pipeline compiled for mode 3's float target.
    ///
    /// Two pipelines rather than a seventh member of `world_pass_target`'s
    /// tuple: a blob is one quad and one shader, and threading it through the
    /// post chain's own pipeline set would be more plumbing than the thing it
    /// carries.
    blob_pipeline_hdr: wgpu::RenderPipeline,
    blobs: wgpu::Buffer,
    blob_count: u32,
    blob_capacity: usize,
    /// The prop pipeline — what a figure is holding — and this frame's boxes.
    ///
    /// Two pipelines for the two colour targets, exactly as the blob has: mode
    /// 3 draws into an HDR offscreen and the other modes into the surface.
    prop_pipeline: wgpu::RenderPipeline,
    prop_pipeline_hdr: wgpu::RenderPipeline,
    props: wgpu::Buffer,
    prop_count: u32,
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
    /// How the finished frame is graded now.
    ///
    /// Mode 3 only — grading lives in the post chain, and the other two modes
    /// have no post chain to put it in. [`grade::Grading`] holds the baked table;
    /// this is the six numbers it was baked from.
    grade: tiamot_core::proto::SkyGrade,
    /// Where fog begins and where it is total, in blocks.
    fog_start: f32,
    fog_end: f32,
    /// How many chunks the last frame actually drew.
    drawn: usize,
    /// How many chunks the last frame drew into the cascades.
    cast: usize,
    /// Skinned figures — every entity, every other player, and now this one.
    skinned: skinned::Skinned,
    /// The hands, drawn last and in view space. See [`viewmodel`].
    hands: viewmodel::Viewmodel,
    /// What they are made of this frame.
    ///
    /// Uploaded in `render` rather than when they are set, because the
    /// projection they are drawn with depends on the frame's aspect ratio and
    /// the caller has no reason to know it.
    hands_at: Vec<viewmodel::Piece>,
    /// The figure pipeline for the swapchain format. Mode 3 keeps its own,
    /// compiled against the float target, in `post`.
    skinned_pipeline: wgpu::RenderPipeline,
    /// The figure pipeline for the shadow cascades.
    ///
    /// `None` until there are cascades to draw into: the cascade uniform's
    /// layout comes from `Shadows`, which mode 3 owns and the other modes do
    /// not allocate at all.
    skinned_shadow: Option<wgpu::RenderPipeline>,
    body_at: Option<[f32; 3]>,
    /// The player's own figure, drawn last so the world pass can leave it out.
    ///
    /// **Their own body, on the same rig everybody else already sees them on.**
    /// Other players arrive as entities and are drawn as `engine:humanoid`; the
    /// local player was a hard-coded box, because their body has to move at
    /// prediction rate rather than at the entity stream's twenty hertz and that
    /// local path never learned the rig. The box was always marked as debug —
    /// "should not grow into one; when Task 12 brings real entities this goes".
    player: bool,
    /// How many of `entities_at` are entities, as opposed to the player.
    entity_figures: usize,
    /// Whether the world pass draws the body, as opposed to only the cascades.
    ///
    /// **Position and visibility are separate on purpose.** In first person the
    /// body is where it is and casts a shadow, and drawing it would put the
    /// inside of the player's own head across the frame.
    body_visible: bool,
    /// Where every entity in view is this frame, camera-relative, in cells.
    ///
    /// Rebuilt each frame from the interpolation buffer, because that is what
    /// it means for an entity to move: nothing here is cached between frames
    /// and nothing needs to be. They ride the same instance array as the
    /// chunks, after the body, so drawing them is an instance index rather than
    /// a second buffer and a second binding.
    entities_at: Vec<skinned::Figure>,
    /// The outline pipeline, and the line segments it draws this frame.
    selection_pipeline: wgpu::RenderPipeline,
    /// The chunk-border overlay: the same line pipeline, its own buffer.
    ///
    /// Separate from the selection because the two change on different clocks —
    /// the outline follows the crosshair every frame, and the borders follow
    /// which chunks are visible.
    borders: wgpu::Buffer,
    border_vertices: u32,
    /// Whether to draw them at all. Off by default: it is a debugging view.
    show_borders: bool,
    /// Fluid sources to outline — see `set_fluid_sources`. Temporary.
    sources: Vec<tiamot_core::BlockPos>,
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
        let fluid_pipeline =
            build_fluid_pipeline(&gpu, &shader, &[Some(&bind_layout)], COLOUR_FORMAT);

        let (blob_pipeline, blob_pipeline_hdr, blobs) = build_blobs(&gpu, &bind_layout);
        let (prop_pipeline, prop_pipeline_hdr, props) = build_props(&gpu, &bind_layout);

        let (selection_shader, selection_pipeline, selection_buffer, border_buffer) =
            build_lines(&gpu, &bind_layout);

        let globals = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("globals"),
            size: size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let sampler = build_atlas_sampler(&gpu);

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

        // The engine's own rig, built in Rust and uploaded once. A mod-supplied
        // model goes through the same constructor — see `core::model`.
        let hands = viewmodel::Viewmodel::new(&gpu, &view, &sampler, COLOUR_FORMAT);
        let skinned = skinned::Skinned::new(&gpu, tiamot_core::model::humanoid());
        let skinned_pipeline =
            skinned::colour_pipeline(&gpu, &skinned, &bind_layout, mode, COLOUR_FORMAT);

        Ok(Self {
            hands,
            gpu,
            skinned,
            skinned_pipeline,
            skinned_shadow: None,
            pipeline,
            fluid_pipeline,
            elapsed: 0.0,
            globals,
            bind_layout,
            bind_group,
            sampler,
            atlas_grid: grid,
            atlas_side: side,
            atlas_view: view,
            chunks: BTreeMap::new(),
            pool: BufferPool::default(),
            selection_pipeline,
            borders: border_buffer,
            border_vertices: 0,
            show_borders: false,
            sources: Vec::new(),
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
            blob_pipeline,
            blob_pipeline_hdr,
            blobs,
            blob_count: 0,
            blob_capacity: BLOB_CAPACITY,
            prop_pipeline,
            prop_pipeline_hdr,
            props,
            prop_count: 0,
            post: None,
            // Full daylight until a sky says otherwise, which is what Task
            // 08's scenes assumed and what a world with no sky mod gets.
            sun_intensity: 1.0,
            sun_colour: [1.0, 1.0, 1.0, 1.0],
            sun_direction: NOON,
            sky_colour: sky_colour(),
            // Ungraded until a sky says otherwise, which keeps a world with no
            // sky mod exactly what it was before grading existed.
            grade: tiamot_core::proto::SkyGrade::NONE,
            // Far enough that nothing fogs until a view distance is set. A
            // client that fogged by default would hide geometry the Task 08
            // scenes assert on.
            fog_start: f32::MAX,
            fog_end: f32::MAX,
            drawn: 0,
            cast: 0,
            entities_at: Vec::new(),
            hands_at: Vec::new(),
            // First person, no player figure, nothing behind it.
            body_at: None,
            player: false,
            entity_figures: 0,
            body_visible: false,
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

    /// Sets how the finished frame is graded, for the frames that follow.
    ///
    /// Takes effect in mode 3 only. Nothing is baked here — [`Renderer::render`]
    /// re-bakes the table when this has moved far enough to reach a pixel, so a
    /// caller may set it every frame at no cost.
    pub const fn set_grade(&mut self, grade: tiamot_core::proto::SkyGrade) {
        self.grade = grade;
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

    /// Whether the world pass draws the body, as opposed to only the cascades.
    ///
    /// Set every frame beside [`Renderer::set_body`]; see [`Renderer::body_visible`].
    pub const fn set_body_visible(&mut self, visible: bool) {
        self.body_visible = visible;
    }

    /// The player's own figure, or `None` for a client not drawing one.
    ///
    /// **Placed last of all the figures**, which is what lets the world pass
    /// leave it out in first person while the cascades keep it — see
    /// [`skinned::Skinned::draw_first`].
    ///
    /// **Must be called after [`Renderer::set_entities`]** for the frame, which
    /// is why it appends rather than being stored beside them: keeping it in
    /// its own field meant cloning the whole entity list once a frame purely to
    /// put one figure on the end, in the frame path, for every frame the client
    /// has ever drawn.
    pub fn set_player(&mut self, figure: Option<skinned::Figure>) {
        self.entities_at.truncate(self.entity_figures);
        self.entities_at.extend(figure);
        self.player = figure.is_some();
    }

    /// Where a named joint of `figure` is, in the figure's own space.
    ///
    /// See [`skinned::Skinned::attachment`]: the caller composes the heading
    /// and the camera-relative offset around it, because those are the
    /// caller's and the joint is the rig's.
    #[must_use]
    pub fn attachment(&self, figure: &skinned::Figure, joint: &str) -> Option<[f32; 16]> {
        self.skinned.attachment(figure, joint)
    }

    /// How many figures the world pass draws, as opposed to the cascades.
    const fn visible_figures(&self, total: usize) -> usize {
        if self.player && !self.body_visible {
            total.saturating_sub(1)
        } else {
            total
        }
    }

    /// Where every entity in view is this frame, camera-relative, in blocks.
    ///
    /// Call before [`Renderer::set_player`], which appends to this.
    pub fn set_entities(&mut self, figures: Vec<skinned::Figure>) {
        self.entity_figures = figures.len();
        self.player = false;
        self.entities_at = figures;
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

    /// How many chunks the last frame drew into the shadow cascades.
    ///
    /// **Larger than [`Renderer::drawn`] whenever something off-screen casts
    /// into view**, which is the whole point of the second list — see
    /// [`Culled`]. Equal to it in the modes with no cascades, where there is
    /// nothing to fill.
    #[must_use]
    pub const fn casting(&self) -> usize {
        self.cast
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
        self.hands.set_atlas(&self.gpu, &view, &self.sampler);
        self.atlas_view = view;
    }

    /// What the hands are holding and how far through a swing they are.
    ///
    /// Given as pieces rather than as state, because deciding what a hand looks
    /// like is `viewmodel`'s and deciding what is in one is the `App`'s — and
    /// the renderer is neither.
    pub fn set_hands(&mut self, pieces: Vec<viewmodel::Piece>) {
        self.hands_at = pieces;
    }

    /// The atlas texture, for an interface that wants to draw a material.
    ///
    /// **The same texture the world is drawn from**, which is the point: a slot
    /// showing stone and a wall made of it cannot disagree about what stone
    /// looks like, because there is one image.
    #[must_use]
    pub const fn atlas_view(&self) -> &wgpu::TextureView {
        &self.atlas_view
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

        // The fluid half, when there is one. Its own buffers from the same
        // pool: a chunk with no milk in it allocates nothing here and the
        // `Option` is what says so, rather than a zero-length buffer that the
        // draw loop would then have to skip every frame.
        let mut fluid = None;
        let mut fluid_bytes = 0;
        if !mesh.has_no_fluid() {
            let vertex_bytes: &[u8] = bytemuck::cast_slice(&mesh.fluid_vertices);
            let index_bytes: &[u8] = bytemuck::cast_slice(&mesh.fluid_indices);
            let vertices = self
                .pool
                .take(&self.gpu, BufferKind::Vertex, vertex_bytes.len() as u64);
            let indices = self
                .pool
                .take(&self.gpu, BufferKind::Index, index_bytes.len() as u64);
            self.gpu.queue.write_buffer(&vertices, 0, vertex_bytes);
            self.gpu.queue.write_buffer(&indices, 0, index_bytes);
            fluid_bytes = (vertex_bytes.len() + index_bytes.len()) as u64;
            fluid = Some(FluidMesh {
                vertices,
                indices,
                index_count: u32::try_from(mesh.fluid_indices.len()).unwrap_or(0),
            });
        }

        self.chunks.insert(
            pos,
            ChunkMesh {
                vertices: vertex_buffer,
                indices: index_buffer,
                index_count: u32::try_from(indices.len()).unwrap_or(0),
                fluid,
                used_bytes: (vertex_bytes.len() + index_bytes.len()) as u64 + fluid_bytes,
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

    /// Sets the blob shadows to draw this frame.
    ///
    /// Each is a centre already lifted clear of the surface it lies on, a
    /// radius, and an opacity — all worked out by the caller, which is the only
    /// thing that knows where the ground under a body is.
    ///
    /// **Every frame, like the entity list.** A blob is where a body is now;
    /// there is nothing to keep between frames.
    pub fn set_blobs(&mut self, blobs: &[([f32; 3], f32, f32)]) {
        let instances: Vec<Blob> = blobs
            .iter()
            .take(self.blob_capacity)
            .map(|(centre, radius, opacity)| Blob {
                centre: [centre[0], centre[1], centre[2], 0.0],
                shape: [*radius, *opacity, 0.0, 0.0],
            })
            .collect();
        self.blob_count = u32::try_from(instances.len()).unwrap_or(0);
        if !instances.is_empty() {
            self.gpu
                .queue
                .write_buffer(&self.blobs, 0, bytemuck::cast_slice(&instances));
        }
    }

    /// Sets what figures are holding this frame.
    ///
    /// **Every frame, like the entity list and the blobs.** A prop hangs off an
    /// animated joint, so where it is is a function of the clip's phase and
    /// changes on every one of them; there is nothing to keep between frames.
    ///
    /// Silently truncated at [`PROP_CAPACITY`], which is the same bargain the
    /// blobs make: a frame that drew a few boxes fewer is better than one that
    /// reallocated a buffer to draw them all.
    pub fn set_props(&mut self, props: &[Prop]) {
        let count = props.len().min(PROP_CAPACITY);
        self.prop_count = u32::try_from(count).unwrap_or(0);
        if count > 0 {
            self.gpu
                .queue
                .write_buffer(&self.props, 0, bytemuck::cast_slice(&props[..count]));
        }
    }

    /// Draws what figures are holding, if anything.
    fn draw_props(&self, pass: &mut wgpu::RenderPass<'_>) {
        if self.prop_count == 0 {
            return;
        }
        let pipeline = if self.post.is_some() {
            &self.prop_pipeline_hdr
        } else {
            &self.prop_pipeline
        };
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_vertex_buffer(0, self.props.slice(..));
        pass.draw(0..36, 0..self.prop_count);
        // **Put the chunks' instance array back**, for exactly the reason the
        // figures put it back: the fluid pass sets slot 0 and the indices and
        // inherits slot 1 from the chunk loop, so leaving anything else there
        // feeds a pond somebody's hand as its chunk offset.
        pass.set_vertex_buffer(1, self.instances.slice(..));
    }

    /// Draws the blob shadows, if there are any.
    fn draw_blobs(&self, pass: &mut wgpu::RenderPass<'_>) {
        if self.blob_count == 0 {
            return;
        }
        let pipeline = if self.post.is_some() {
            &self.blob_pipeline_hdr
        } else {
            &self.blob_pipeline
        };
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_vertex_buffer(0, self.blobs.slice(..));
        // Six vertices built in the vertex stage from `vertex_index`; the
        // instance is the only per-body data.
        pass.draw(0..6, 0..self.blob_count);
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
            push_box_edges(&mut vertices, *cell, 1.0);
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

    /// Turns the chunk-border overlay on or off.
    ///
    /// Costs nothing while off: the geometry is built during the frame that
    /// draws it, from the chunks the culler has already decided are visible.
    pub const fn set_chunk_borders(&mut self, show: bool) {
        self.show_borders = show;
    }

    /// Whether the chunk-border overlay is on.
    #[must_use]
    pub const fn chunk_borders(&self) -> bool {
        self.show_borders
    }

    /// How many line vertices the border overlay drew last frame.
    ///
    /// For the test that ties it to the culled set: a cage that draws the wrong
    /// number of boxes is a cage around the wrong thing, and no screenshot of a
    /// grid-textured floor will say so.
    #[must_use]
    pub const fn chunk_border_vertices(&self) -> u32 {
        self.border_vertices
    }

    /// Builds the border boxes for the chunks about to be drawn.
    ///
    /// In CELLS, like everything the selection shader takes — it divides by three
    /// on the way to clip space — so a chunk is `CHUNK_SUBNODES` on a side rather
    /// than `CHUNK_BLOCKS`. Getting that wrong draws a cage a third of the size
    /// of the thing it is supposed to be outlining, which looks like a bug in the
    /// culler rather than in a constant.
    fn upload_chunk_borders(&mut self, camera: &Camera, visible: &[(ChunkPos, u32)]) {
        if !self.show_borders && self.sources.is_empty() {
            self.border_vertices = 0;
            return;
        }

        let side = f32::from(u16::try_from(tiamot_core::CHUNK_SUBNODES).unwrap_or(48));
        let mut vertices: Vec<[f32; 3]> = Vec::with_capacity(visible.len() * 24);

        // **Fluid sources, cased in the same wireframe.** A source and a full
        // flow block are the same colour and the same height, so from inside a
        // pond there is no way to see which block is feeding it — which is the
        // one thing you want to know while building the rest of this. Shares
        // the border overlay's buffer and shader rather than growing a second
        // of each for a temporary aid.
        let cells = tiamot_core::SUBNODES_PER_AXIS as f32;
        for at in &self.sources {
            // Chunk offset plus the block's position inside it, both in cells,
            // which is what the selection shader takes.
            let chunk = at.chunk();
            let base = camera.position.chunk_offset(chunk);
            let local = at.local();
            push_box_edges(
                &mut vertices,
                [
                    base.x + local.x as f32 * cells,
                    base.y + local.y as f32 * cells,
                    base.z + local.z as f32 * cells,
                ],
                cells,
            );
        }

        if !self.show_borders {
            self.border_vertices = u32::try_from(vertices.len()).unwrap_or(0);
            if !vertices.is_empty() {
                self.gpu
                    .queue
                    .write_buffer(&self.borders, 0, bytemuck::cast_slice(&vertices));
            }
            return;
        }
        for (pos, _) in visible {
            let offset = camera.position.chunk_offset(*pos);
            push_box_edges(
                &mut vertices,
                [offset.x * cells, offset.y * cells, offset.z * cells],
                side,
            );
            if vertices.len() >= CHUNK_BORDER_CAPACITY {
                vertices.truncate(CHUNK_BORDER_CAPACITY);
                break;
            }
        }

        self.border_vertices = u32::try_from(vertices.len()).unwrap_or(0);
        if !vertices.is_empty() {
            self.gpu
                .queue
                .write_buffer(&self.borders, 0, bytemuck::cast_slice(&vertices));
        }
    }

    /// The fluid sources to outline, in world blocks.
    ///
    /// Temporary, and deliberately shaped so removing it is deleting a field:
    /// it is a tracking aid for building the rest of Task 11, not a feature.
    pub fn set_fluid_sources(&mut self, sources: Vec<tiamot_core::BlockPos>) {
        self.sources = sources;
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
            fluid: [self.elapsed, 0.0, 0.0, 0.0],
        }
    }

    /// Advances the clock that milk scrolls by, in seconds.
    ///
    /// **Wrapped, and that is not tidiness.** An `f32` holding hours of seconds
    /// has coarser steps than a frame is long, so a texture offset computed from
    /// it stops advancing smoothly and a river starts to judder — hours into a
    /// session, on the machine of whoever was still playing. Wrapping at a whole
    /// number of scroll cycles keeps the value small and the seam invisible,
    /// because the UV is taken modulo one anyway.
    pub fn advance_clock(&mut self, dt: f32) {
        if dt.is_finite() {
            self.elapsed = (self.elapsed + dt) % FLUID_CLOCK_WRAP;
        }
    }
}

/// What one frame's culling decided.
///
/// # Two lists, because a shadow's caster need not be on screen
///
/// **The cascades used to be filled from the camera-visible list**, so a wall
/// stopped shadowing the ground the moment it left the frustum — turn away from
/// a tower and its shadow vanished mid-stride. Reported from the window as a
/// shadow disappearing abruptly when you look away from what casts it, and the
/// diagnosis was right there in the report: it is not the chunk being unloaded,
/// it is the chunk not being drawn into the light's map.
///
/// `shadow::CASTER_MARGIN` was written for exactly this and could never fix it:
/// it widens the light's box behind the camera, which is no help when the
/// geometry never reaches the pass at all.
#[derive(Debug, Default)]
struct Culled {
    /// Chunks the world pass draws, with their instance index.
    visible: Vec<(ChunkPos, u32)>,
    /// Chunks the cascades draw, with their instance index.
    ///
    /// Tested against each cascade's own matrix — the same `Frustum` the camera
    /// uses, against the light's box rather than the eye's — so anything that
    /// can cast into the shadow map is in it whether or not it is on screen.
    casters: Vec<(ChunkPos, u32)>,
    /// Where the player's own body sits in the instance array.
    body: u32,
}

impl Renderer {
    /// Culls the resident chunks against the camera AND the light, and uploads
    /// their offsets.
    ///
    /// Both lists index one instance array, which is why this is one function:
    /// a draw loop indexes that array by the number recorded here, so the list
    /// and the array must not be built separately.
    fn cull_and_upload(&mut self, camera: &Camera, view_projection: glam::Mat4) -> Culled {
        // Cull, then build the instance array. Both in one pass so a chunk's
        // offset is computed exactly once per frame.
        let frustum = Frustum::from_view_projection(view_projection);
        // **The light's frusta as well as the camera's**, because a shadow's
        // caster does not have to be on screen. See `Culled::casters`.
        let cascades: Vec<Frustum> = self
            .post
            .as_ref()
            .and_then(graph::Post::shadows)
            .map(|shadows| {
                shadows
                    .matrices()
                    .iter()
                    .map(|matrix| Frustum::from_view_projection(*matrix))
                    .collect()
            })
            .unwrap_or_default();

        let mut culled = Culled::default();
        let mut instances = Vec::with_capacity(self.chunks.len());
        for pos in self.chunks.keys() {
            let offset = camera.position.chunk_offset(*pos);
            let seen = frustum.contains_chunk(offset);
            let casts = cascades
                .iter()
                .any(|cascade| cascade.contains_chunk(offset));
            if !seen && !casts {
                continue;
            }
            // **One instance array, two index lists.** Both passes read the
            // same offsets, so a chunk that is both seen and casting is
            // uploaded once — and a chunk that is only casting still has an
            // instance for the cascade pass to point at.
            //
            // Positions rather than the meshes themselves: this method takes
            // `&mut self` to grow the instance buffer, and a borrow of a mesh
            // would keep that borrow alive across the draw loop. The lookup it
            // costs is one `BTreeMap` probe per drawn chunk per frame.
            let instance = instances.len() as u32;
            if seen {
                culled.visible.push((*pos, instance));
            }
            if casts {
                culled.casters.push((*pos, instance));
            }
            instances.push(Instance {
                offset: [offset.x, offset.y, offset.z, 0.0],
            });
        }
        self.drawn = culled.visible.len();
        self.cast = culled.casters.len();

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
        culled.body = instances.len().saturating_sub(1) as u32;

        culled
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
                skinned: &self.skinned,
            },
            graph::Setup {
                mode: self.mode,
                width: size.0,
                height: size.1,
                shadow_texels: self.shadow_quality.texels(),
            },
        ));
        // The figure pipeline for the cascades, which can only be built once
        // there are cascades: its layout comes from `Shadows`, and the modes
        // that allocate none have nothing to compile against.
        self.skinned_shadow = self
            .post
            .as_ref()
            .and_then(graph::Post::shadows)
            .map(|shadows| skinned::shadow_pipeline(&self.gpu, &self.skinned, shadows.layout()));
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
                grade: self.grade,
            },
        );
    }

    /// Where the world pass draws, and with which pipelines.
    ///
    /// Straight to the target in modes 1 and 2, and into the float scene texture
    /// in mode 3 so the post chain has something with headroom in it to read.
    fn world_pass_target<'a>(
        &'a self,
        target: &'a wgpu::TextureView,
    ) -> (
        &'a wgpu::TextureView,
        &'a wgpu::TextureView,
        &'a wgpu::RenderPipeline,
        &'a wgpu::RenderPipeline,
        &'a wgpu::RenderPipeline,
        &'a wgpu::RenderPipeline,
    ) {
        match self.post.as_ref() {
            Some(post) => {
                let (scene, depth) = post.scene_target();
                (
                    scene,
                    depth,
                    post.world_pipeline(),
                    post.fluid_pipeline(),
                    post.selection_pipeline(),
                    post.skinned_pipeline(),
                )
            }
            None => (
                target,
                &self.depth,
                &self.pipeline,
                &self.fluid_pipeline,
                &self.selection_pipeline,
                &self.skinned_pipeline,
            ),
        }
    }

    /// Draws every visible chunk's milk, after all of the opaque geometry.
    ///
    /// **After, and in one sweep, rather than per chunk as it is met.**
    /// Transparency composites against what is already in the target, so the
    /// whole opaque world has to be there first — interleaving would blend a
    /// pond against whichever chunks happened to be drawn before it, and the
    /// answer would change as the camera turned.
    ///
    /// The fluid pipeline does not write depth, so milk behind milk is not
    /// occluded by milk in front of it. What that costs is that two fluid
    /// surfaces seen through one another are not sorted against each other,
    /// which for one fluid of one colour is invisible and for two would not be.
    fn draw_fluid(
        &self,
        pass: &mut wgpu::RenderPass<'_>,
        pipeline: &wgpu::RenderPipeline,
        visible: &[(ChunkPos, u32)],
    ) {
        let mut wet = false;
        for (pos, instance) in visible {
            let Some(fluid) = self.chunks.get(pos).and_then(|mesh| mesh.fluid.as_ref()) else {
                continue;
            };
            if !wet {
                // Set once, and only when there is milk in view at all: a dry
                // world must not pay a pipeline switch every frame for nothing.
                pass.set_pipeline(pipeline);
                wet = true;
            }
            pass.set_vertex_buffer(0, fluid.vertices.slice(..));
            pass.set_index_buffer(fluid.indices.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..fluid.index_count, 0, *instance..*instance + 1);
        }
    }

    /// Draws every shadow CASTER into every cascade.
    ///
    /// The same meshes and the same instance buffer as the world pass, drawn
    /// from the sun instead of from the eye — which is why the shadow pipeline
    /// shares this module's vertex layout rather than having one of its own.
    /// Nothing happens in the modes with no cascades to fill.
    ///
    /// **Casters, not the visible list**, which is what [`Culled`] is about: a
    /// wall that leaves the screen still shadows the ground in front of you.
    fn fill_cascades(&self, encoder: &mut wgpu::CommandEncoder, culled: &Culled) {
        let Some(shadows) = self.post.as_ref().and_then(graph::Post::shadows) else {
            return;
        };

        // **One sweep, not two, and that is the whole of this function's
        // history.**
        //
        // Figures need a different pipeline from terrain, so this used to make
        // a second `shadows.render` call for them. Every call begins a render
        // pass per cascade with `LoadOp::Clear`, so the second sweep wiped the
        // depth the first had just written — and the cascades ended up holding
        // the mobs and nothing else.
        //
        // Reported from the window exactly as that reads: "the stalker mob DOES
        // have a shadow", a built tower does not, and neither does the player's
        // own body. It only happened with a figure on screen, which is why the
        // offscreen shadow tests — which draw no figures, so the second sweep
        // never ran — were all green while the game had no terrain shadows at
        // all.
        //
        // Two pipelines inside one pass is the fix and also the safer shape:
        // there is no second `render` call to get the load op wrong in.
        shadows.render(encoder, |pass, cascade| {
            pass.set_vertex_buffer(1, self.instances.slice(..));
            for (pos, instance) in &culled.casters {
                let Some(mesh) = self.chunks.get(pos) else {
                    continue;
                };
                pass.set_vertex_buffer(0, mesh.vertices.slice(..));
                pass.set_index_buffer(mesh.indices.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..mesh.index_count, 0, *instance..*instance + 1);
            }
            // Figures, in their own pipeline. **A mob with no shadow floats**,
            // which is the one thing about a drawn body that everybody notices
            // immediately — and it is what led to the two sweeps above.
            if self.skinned.drawn() > 0
                && let Some(skinned_shadow) = self.skinned_shadow.as_ref()
                && let Some(bind) = shadows.cascade_bind(cascade)
            {
                pass.set_pipeline(skinned_shadow);
                pass.set_bind_group(1, bind, &[]);
                self.skinned.draw(pass);
            }
        });
    }

    /// Draws the hands, after the post chain and onto the target itself.
    ///
    /// **Not inside the world pass**, for two reasons. Mode 3's world pass
    /// writes a float scene texture, and every pipeline that draws into it
    /// needs a second variant for that format — a hand is not worth two
    /// pipelines. And a viewmodel put through bloom and a filmic tonemap would
    /// change brightness with whatever the player happened to be looking at,
    /// which is exactly what a fixed piece of the interface must not do. So it
    /// lands where the HUD lands: mode-independent, on a finished frame.
    fn draw_hands(&self, encoder: &mut wgpu::CommandEncoder, target: &wgpu::TextureView) {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("viewmodel"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    // LOAD: the world is already here.
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        self.hands.draw(&mut pass);
        self.hands.draw(&mut pass);
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

        // And before the chain runs, because baking uploads a texture the
        // composite is about to read. Cheap when the sky has not moved: the
        // table is rebuilt only when the grade changes by enough to reach a
        // pixel, which over a twenty-minute day is a few times a second rather
        // than sixty.
        if let Some(post) = self.post.as_mut() {
            let grade = self.grade;
            post.bake_grade(&self.gpu, &grade);
        }

        self.gpu.queue.write_buffer(
            &self.globals,
            0,
            bytemuck::bytes_of(&self.globals_for(view_projection)),
        );

        // Their own projection, and no view matrix at all — a hand is at a
        // place on the screen rather than a place in the world.
        let hands = std::mem::take(&mut self.hands_at);
        self.hands
            .prepare(&self.gpu, camera.projection(aspect), &hands);
        self.hands_at = hands;

        let culled = self.cull_and_upload(camera, view_projection);
        self.upload_chunk_borders(camera, &culled.visible);

        // **Once per frame, before any pass.** All three read the same
        // instances and the same palette, which is what makes a figure appear
        // in the same place in the world and in its own shadow — posing per
        // pass would let the two disagree by a frame.
        let figures = std::mem::take(&mut self.entities_at);
        self.skinned.prepare(&self.gpu, &figures);
        self.entities_at = figures;

        let mut encoder = self
            .gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame"),
            });

        self.fill_cascades(&mut encoder, &culled);

        let (colour, depth, world_pipeline, fluid_pipeline, selection_pipeline, skinned_pipeline) =
            self.world_pass_target(target);

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

            for (pos, instance) in &culled.visible {
                let Some(mesh) = self.chunks.get(pos) else {
                    continue;
                };
                pass.set_vertex_buffer(0, mesh.vertices.slice(..));
                pass.set_index_buffer(mesh.indices.slice(..), wgpu::IndexFormat::Uint32);
                // The instance range picks this chunk's offset out of the
                // shared array — one buffer write per frame instead of one per
                // chunk.
                pass.draw_indexed(0..mesh.index_count, 0, *instance..*instance + 1);
            }

            // **After the terrain, before the fluid.** A blob is a mark on the
            // ground, so it goes over what it is marking; milk is above the
            // ground and should be able to cover it.
            self.draw_blobs(&mut pass);

            // Figures, in their own pipeline. After the terrain because they
            // are opaque and depth-tested either way, and before the fluid
            // because the fluid is blended and has to come last.
            if self.skinned.drawn() > 0 {
                pass.set_pipeline(skinned_pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                self.skinned
                    .draw_first(&mut pass, self.visible_figures(self.skinned.drawn()));

                // **Put the chunks' instance array back.** A figure's instances
                // live in slot 1 too, and `draw_fluid` below sets only slot 0
                // and the indices — it inherits slot 1 from the chunk loop
                // above, which is the whole reason one buffer serves every
                // chunk. Leaving the figures' buffer bound there fed each pond
                // a figure's position as its chunk offset, and reported as
                // "fluid rendered in the sky".
                pass.set_vertex_buffer(1, self.instances.slice(..));
            }

            // What those figures are holding, in the same company: opaque,
            // depth-tested, after the terrain and before the blended fluid.
            self.draw_props(&mut pass);

            self.draw_fluid(&mut pass, fluid_pipeline, &culled.visible);

            // Last, so it draws over the world it outlines. Its pipeline does
            // not write depth, so the order within the pass is what decides
            // this rather than the depth buffer.
            if self.selection_vertices > 0 {
                pass.set_pipeline(selection_pipeline);
                pass.set_vertex_buffer(0, self.selection.slice(..));
                pass.draw(0..self.selection_vertices, 0..1);
            }

            // And the chunk cage over that, when it is asked for.
            if self.border_vertices > 0 {
                pass.set_pipeline(selection_pipeline);
                pass.set_vertex_buffer(0, self.borders.slice(..));
                pass.draw(0..self.border_vertices, 0..1);
            }
        }

        // The post chain, if this mode has one. It reads the scene texture the
        // pass above just wrote and lands on `target`, so from outside the
        // renderer a frame looks the same in every mode.
        self.run_post(&mut encoder, target, view_projection);

        self.draw_hands(&mut encoder, target);

        self.gpu.queue.submit(Some(encoder.finish()));
    }
}

/// Appends the twelve edges of a unit cube at `corner`, as line-list vertices.
///
/// Twelve segments, twenty-four vertices. A line *strip* would need degenerate
/// segments to jump between the disconnected edges of a cube, and those show up
/// as stray diagonals the moment anything upstream reorders them.
fn push_box_edges(out: &mut Vec<[f32; 3]>, corner: [f32; 3], size: f32) {
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
        [x + size, y, z],
        [x + size, y, z + size],
        [x, y, z + size],
        [x, y + size, z],
        [x + size, y + size, z],
        [x + size, y + size, z + size],
        [x, y + size, z + size],
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

/// Most blob shadows one frame will draw.
///
/// One per body in view. Far more than a populated area holds, and a hard
/// ceiling on a buffer that is written every frame from a list a server
/// controls the length of.
const BLOB_CAPACITY: usize = 512;

/// Most prop boxes one frame will draw.
///
/// A held item is at most twenty-seven cells and there are two hands, so this
/// is a couple of dozen figures' worth. The local player is the only one whose
/// hands the client knows about today; the cap is what stops a future one that
/// knows about everybody from being unbounded.
const PROP_CAPACITY: usize = 512;

/// One prop box, as the shader reads it.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Prop {
    /// The model matrix, camera-relative, by column.
    pub model: [f32; 16],
    /// The atlas rectangle: `u0, v0, u1, v1`.
    pub uv: [f32; 4],
}

/// How big a held block is, as a half-extent in cells.
///
/// **Much smaller than a real block.** A full one at true scale is nearly a
/// third of the figure's height and reads as somebody carrying a wardrobe.
/// This is about a fifth of a block across — halved from the first attempt,
/// which was reported from the window as still too big. Tuned by eye, like the
/// viewmodel's own constants and for the same reason: a held object is a
/// composition, and the only test of the number is whether it looks held.
const HELD_HALF: f32 = 0.35;

/// Where the block sits in the hand joint's own frame, in cells.
///
/// The rig hangs each limb downward from its joint — a hand's box runs from the
/// joint origin to about a cell below it — so this drops the block to the fist
/// and pushes it a little forward of the palm. It deliberately overlaps the
/// hand rather than floating clear of it, which is the same call the viewmodel
/// made and was reported as looking better than a gap.
const HELD_GRIP: [f32; 3] = [0.0, -0.55, 0.35];

/// The boxes one hand is holding, as instances the prop pass can draw.
///
/// `joint` is the hand's transform in the figure's own space — cells, model
/// space, straight from [`Renderer::attachment`] — and the figure's heading and
/// camera-relative offset are composed around it here, in the same order the
/// skinned vertex stage composes them: cells to blocks, then the heading, then
/// the offset. Getting that order wrong puts a held block somewhere in the
/// world that has nothing to do with the hand.
///
/// A cut is one box per occupied cell, exactly as the viewmodel draws it, so a
/// stair looks like a stair in both views.
#[must_use]
pub fn held_boxes(
    figure: &skinned::Figure,
    joint: &[f32; 16],
    shape: u32,
    uv: [f32; 4],
) -> Vec<Prop> {
    use glam::Mat4;

    let placed = Mat4::from_translation(glam::Vec3::from(figure.offset))
        * Mat4::from_rotation_y(figure.yaw)
        // The rig is built in cells and the world is in blocks — the division
        // `place()` does in the shader, done here for the same geometry.
        * Mat4::from_scale(glam::Vec3::splat(1.0 / 3.0))
        * Mat4::from_cols_array(joint)
        * Mat4::from_translation(glam::Vec3::from(HELD_GRIP));

    let whole = shape == 0 || shape == tiamot_core::inventory::Shape::ALL;
    if whole {
        return vec![Prop {
            model: (placed * Mat4::from_scale(glam::Vec3::splat(HELD_HALF))).to_cols_array(),
            uv,
        }];
    }

    let half = HELD_HALF / 3.0;
    let mut boxes = Vec::new();
    for z in 0..3 {
        for y in 0..3 {
            for x in 0..3 {
                if shape & (1 << (x + y * 3 + z * 9)) == 0 {
                    continue;
                }
                // The middle cell at the centre and the outer two a full cell
                // either side, so three of them fill exactly the block one box
                // would have.
                let along = |index: i32| (index - 1) as f32 * 2.0 * half;
                let cell = Mat4::from_translation(glam::vec3(along(x), along(y), along(z)))
                    * Mat4::from_scale(glam::Vec3::splat(half));
                boxes.push(Prop {
                    model: (placed * cell).to_cols_array(),
                    uv,
                });
            }
        }
    }
    boxes
}

/// One blob shadow, as the shader reads it.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Blob {
    /// Camera-relative position of the disc's centre, in blocks. w unused.
    centre: [f32; 4],
    /// Radius in blocks in x, opacity in y. Two spare, because a vertex
    /// attribute is a `vec4` either way.
    shape: [f32; 4],
}

/// Everything the blob pass needs: two pipelines and a buffer.
///
/// Grouped into one function because `Renderer::new` went over clippy's line
/// ceiling again — the sixth time this task. The shader is not kept: nothing
/// recompiles a blob pipeline after startup, since both target formats are
/// known here.
fn build_blobs(
    gpu: &Gpu,
    bind_layout: &wgpu::BindGroupLayout,
) -> (wgpu::RenderPipeline, wgpu::RenderPipeline, wgpu::Buffer) {
    let shader = gpu
        .device
        .create_shader_module(wgpu::include_wgsl!("blob.wgsl"));
    let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("blob-shadows"),
        size: (BLOB_CAPACITY * size_of::<Blob>()) as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    (
        build_blob_pipeline(gpu, &shader, bind_layout, COLOUR_FORMAT),
        build_blob_pipeline(gpu, &shader, bind_layout, graph::HDR_FORMAT),
        buffer,
    )
}

/// How the atlas is sampled.
///
/// Its own function because `Renderer::new` sits at clippy's line ceiling and
/// this is a block of settings with one reason to change — the same argument
/// `build_lines` and `build_blobs` make.
fn build_atlas_sampler(gpu: &Gpu) -> wgpu::Sampler {
    gpu.device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("atlas"),
        // Repeat, because the shader's `fract` already wraps — this only
        // matters for the derivative-driven mip levels at grazing angles.
        address_mode_u: wgpu::AddressMode::Repeat,
        address_mode_v: wgpu::AddressMode::Repeat,
        address_mode_w: wgpu::AddressMode::Repeat,
        // NEAREST magnification: voxel textures are pixel art, and smoothing
        // them is what turns a 16-pixel tile into a smear. Linear minification
        // and mip interpolation, because the alternative is aliasing that
        // shimmers as the camera moves.
        mag_filter: wgpu::FilterMode::Nearest,
        min_filter: wgpu::FilterMode::Linear,
        mipmap_filter: wgpu::MipmapFilterMode::Linear,
        ..Default::default()
    })
}

/// The prop pass: two pipelines and the buffer they read.
///
/// Built together for the reason `build_blobs` gives — `Renderer::new` sits at
/// clippy's line ceiling, and this is three statements that belong to one
/// thing.
fn build_props(
    gpu: &Gpu,
    bind_layout: &wgpu::BindGroupLayout,
) -> (wgpu::RenderPipeline, wgpu::RenderPipeline, wgpu::Buffer) {
    let shader = gpu
        .device
        .create_shader_module(wgpu::include_wgsl!("prop.wgsl"));
    let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("props"),
        size: (PROP_CAPACITY * size_of::<Prop>()) as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    (
        build_prop_pipeline(gpu, &shader, bind_layout, COLOUR_FORMAT),
        build_prop_pipeline(gpu, &shader, bind_layout, graph::HDR_FORMAT),
        buffer,
    )
}

/// The prop pipeline: one instanced cube per box, opaque.
///
/// **Depth-writing, unlike the blob.** A held block is a solid object standing
/// in the world: the arm holding it goes in front of it from some angles and
/// behind it from others, and only the depth buffer knows which.
fn build_prop_pipeline(
    gpu: &Gpu,
    shader: &wgpu::ShaderModule,
    bind_layout: &wgpu::BindGroupLayout,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    let layout = gpu
        .device
        .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("prop-pipeline-layout"),
            bind_group_layouts: &[Some(bind_layout)],
            immediate_size: 0,
        });
    let columns: [wgpu::VertexAttribute; 5] = std::array::from_fn(|index| wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x4,
        offset: (index * size_of::<[f32; 4]>()) as u64,
        shader_location: index as u32,
    });

    gpu.device
        .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("prop"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: shader,
                entry_point: Some("vertex_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: size_of::<Prop>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &columns,
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: shader,
                entry_point: Some("fragment_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::COLOR,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: Some(wgpu::Face::Back),
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

/// The blob-shadow pipeline: one instanced quad per body, blended dark.
///
/// **Alpha-blended and depth-tested but not depth-writing**, like the fluid
/// pass and for the same reason: it is a mark on a surface rather than a
/// surface, so it must not occlude anything drawn after it.
fn build_blob_pipeline(
    gpu: &Gpu,
    shader: &wgpu::ShaderModule,
    bind_layout: &wgpu::BindGroupLayout,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    let layout = gpu
        .device
        .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("blob-pipeline-layout"),
            bind_group_layouts: &[Some(bind_layout)],
            immediate_size: 0,
        });

    gpu.device
        .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("blob"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: shader,
                entry_point: Some("vertex_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: size_of::<Blob>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &[
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x4,
                            offset: 0,
                            shader_location: 0,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x4,
                            offset: size_of::<[f32; 4]>() as u64,
                            shader_location: 1,
                        },
                    ],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: shader,
                entry_point: Some("fragment_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::COLOR,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                // Seen from below as well as above: a player standing on glass
                // over a drop should still be grounded to whoever is under it.
                cull_mode: None,
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(false),
                depth_compare: Some(wgpu::CompareFunction::Less),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        })
}

/// The line overlay: one pipeline, and the two buffers that feed it.
///
/// The selection cage and the chunk cage are the same shader with different
/// vertices, so they are built together — and `Renderer::new` is at clippy's
/// line ceiling, which is what makes this a function rather than eight lines
/// inline.
fn build_lines(
    gpu: &Gpu,
    bind_layout: &wgpu::BindGroupLayout,
) -> (
    wgpu::ShaderModule,
    wgpu::RenderPipeline,
    wgpu::Buffer,
    wgpu::Buffer,
) {
    let shader = gpu
        .device
        .create_shader_module(wgpu::include_wgsl!("selection.wgsl"));
    let pipeline = build_selection_pipeline(gpu, &shader, bind_layout, COLOUR_FORMAT);
    (
        shader,
        pipeline,
        line_buffer(gpu, "selection", SELECTION_CAPACITY),
        line_buffer(gpu, "chunk-borders", CHUNK_BORDER_CAPACITY),
    )
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

/// The same, for the twelve-byte fluid vertex: one more word at location 3.
fn fluid_vertex_layout() -> [wgpu::VertexBufferLayout<'static>; 2] {
    const VERTEX_ATTRIBUTES: [wgpu::VertexAttribute; 3] = [
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
        wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Uint32,
            offset: 8,
            shader_location: 3,
        },
    ];
    const INSTANCE_ATTRIBUTES: [wgpu::VertexAttribute; 1] = [wgpu::VertexAttribute {
        format: wgpu::VertexFormat::Float32x4,
        offset: 0,
        shader_location: 2,
    }];

    [
        wgpu::VertexBufferLayout {
            array_stride: size_of::<crate::mesher::FluidVertex>() as u64,
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

/// A vertex buffer of line endpoints, written every frame.
///
/// The selection outline and the chunk borders want the same thing and were
/// two copies of it; extracted when `Renderer::new` went over clippy's line
/// ceiling, which is the fifth time this task that appending to a long function
/// was the wrong move.
fn line_buffer(gpu: &Gpu, label: &'static str, capacity: usize) -> wgpu::Buffer {
    gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: (capacity * size_of::<[f32; 3]>()) as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// Where the sun sits before a sky mod says otherwise: down, and a little to
/// one side.
///
/// Straight down would give every vertical face the same light and collapse
/// every shadow to nothing, which is the same reason `client::sky` tilts its
/// arc. The two numbers agree deliberately.
const NOON: [f32; 3] = [0.0, -0.970_142_5, 0.242_535_62];

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

/// The fluid pipeline: the same world shader, blended, over its own vertex.
///
/// # Why fluid is a separate pipeline rather than a flag
///
/// Three things differ, and none of them can be a uniform.
///
/// **Blending is pipeline state.** A transparent surface has to be composited
/// against what is already in the target, and that is `ColorTargetState::blend`,
/// which is fixed when the pipeline is built.
///
/// **Depth writes are off.** Milk still TESTS against the depth buffer — a pond
/// behind a hill is hidden by the hill — but it must not write, or the nearer
/// face of a pond would occlude its own far face and a swimmer would see a hole
/// where the bottom should be. This is why the fluid pass runs after all the
/// opaque geometry rather than interleaved with it.
///
/// **The vertex is wider.** A fluid vertex carries where the surface really sits
/// and which way it is running (see `mesher::FluidVertex`), which terrain has no
/// use for and should not pay four bytes a vertex to carry.
///
/// Back faces are NOT culled. From inside a pond the near surface is a back face
/// and it is exactly what a swimmer is looking through; culling it is what made
/// being underwater look like being in air.
fn build_fluid_pipeline(
    gpu: &Gpu,
    shader: &wgpu::ShaderModule,
    bind_layouts: &[Option<&wgpu::BindGroupLayout>],
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    let layout = gpu
        .device
        .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("fluid-pipeline-layout"),
            bind_group_layouts: bind_layouts,
            immediate_size: 0,
        });

    gpu.device
        .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("fluid"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: shader,
                entry_point: Some("fluid_vertex"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &fluid_vertex_layout(),
            },
            fragment: Some(wgpu::FragmentState {
                module: shader,
                entry_point: Some("fluid_fragment"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
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
                depth_compare: Some(wgpu::CompareFunction::Less),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A joint sitting three cells to the figure's own right, at rest.
    fn joint(at: [f32; 3]) -> [f32; 16] {
        let mut matrix = tiamot_core::model::Matrix::from([
            1.0, 0.0, 0.0, 0.0, //
            0.0, 1.0, 0.0, 0.0, //
            0.0, 0.0, 1.0, 0.0, //
            0.0, 0.0, 0.0, 1.0_f32,
        ]);
        matrix[12] = at[0];
        matrix[13] = at[1];
        matrix[14] = at[2];
        matrix
    }

    fn placement(prop: &Prop) -> glam::Vec3 {
        glam::vec3(prop.model[12], prop.model[13], prop.model[14])
    }

    #[test]
    fn a_held_box_lands_where_the_hand_is_and_not_where_the_figure_started() {
        // **The composition order, which is the whole of what can go wrong
        // here.** The rig is in cells and the world is in blocks, and the
        // figure's own offset is ALREADY in blocks — so scaling the wrong side
        // of the multiply divides a player's position by three and puts their
        // hands a third of the way back to the origin. Nothing about the
        // picture says which of the two happened; the block is simply somewhere
        // else.
        let figure = skinned::Figure {
            offset: [12.0, -3.0, 40.0],
            yaw: 0.0,
            anim: 0,
            phase: 0.0,
            carrying: [false; 2],
        };
        let boxes = held_boxes(&figure, &joint([3.0, 0.0, 0.0]), 0, [0.0; 4]);
        assert_eq!(boxes.len(), 1, "a whole block is one box");

        // At yaw zero the figure's axes are the world's, so the hand is its
        // three cells — one block — along x, plus the grip, and the offset is
        // untouched.
        let grip = glam::Vec3::from(HELD_GRIP) / 3.0;
        let wanted = glam::Vec3::from(figure.offset) + glam::vec3(1.0, 0.0, 0.0) + grip;
        let at = placement(&boxes[0]);
        assert!(
            (at - wanted).length() < 1e-5,
            "the hand is at {at} and should be at {wanted}"
        );
    }

    #[test]
    fn a_held_box_turns_with_the_figure_and_the_offset_does_not() {
        // Turned a quarter, so the hand swings from +x to −z while the body
        // stays exactly where it was. A rotation applied to the wrong side —
        // to the whole thing rather than to the rig — would move the body too,
        // which is a player teleporting as they turn.
        let figure = skinned::Figure {
            offset: [12.0, -3.0, 40.0],
            yaw: std::f32::consts::FRAC_PI_2,
            anim: 0,
            phase: 0.0,
            carrying: [false; 2],
        };
        let boxes = held_boxes(&figure, &joint([3.0, 0.0, 0.0]), 0, [0.0; 4]);
        let at = placement(&boxes[0]);

        // The hand's own displacement from the body, which is what turned.
        let arm = at - glam::Vec3::from(figure.offset);
        assert!(
            arm.z < -0.5 && arm.x.abs() < 0.5,
            "a quarter turn should carry the hand from +x to −z, and it is at {arm}"
        );
        // One block out, plus the grip: the arm did not stretch or collapse.
        assert!(
            (arm.length() - 1.0).abs() < 0.5,
            "the hand is {} blocks from the body: {arm}",
            arm.length()
        );
    }

    #[test]
    fn a_held_cut_is_one_box_per_cell_and_fills_the_block_it_came_from() {
        let figure = skinned::Figure {
            offset: [0.0; 3],
            yaw: 0.0,
            anim: 0,
            phase: 0.0,
            carrying: [false; 2],
        };
        let mask = 0b111 << 12;
        let cut = held_boxes(&figure, &joint([0.0; 3]), mask, [0.0; 4]);
        assert_eq!(cut.len(), mask.count_ones() as usize);

        // Three cells across is exactly the block one box would have been, in
        // the figure's own frame — measured on x, which at yaw zero is the
        // world's x and the row's own axis.
        let extent = |boxes: &[Prop]| {
            let mut low = f32::MAX;
            let mut high = f32::MIN;
            for prop in boxes {
                // The scale is on the diagonal and is already in blocks —
                // the cells-to-blocks division is inside the matrix.
                let half = prop.model[0].abs();
                low = low.min(placement(prop).x - half);
                high = high.max(placement(prop).x + half);
            }
            (low, high)
        };
        let whole = held_boxes(&figure, &joint([0.0; 3]), 0, [0.0; 4]);
        let (cut_low, cut_high) = extent(&cut);
        let (whole_low, whole_high) = extent(&whole);
        assert!(
            (cut_low - whole_low).abs() < 1e-5 && (cut_high - whole_high).abs() < 1e-5,
            "a full row of cells spans {cut_low}..{cut_high} and the block spans \
             {whole_low}..{whole_high}"
        );

        // A mask with every cell is the block again — one box, not
        // twenty-seven that look like one.
        let all = held_boxes(
            &figure,
            &joint([0.0; 3]),
            tiamot_core::inventory::Shape::ALL,
            [0.0; 4],
        );
        assert_eq!(all.len(), 1);
    }
}
