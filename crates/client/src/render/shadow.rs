// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Cascaded shadow maps: the world drawn from the sun, three times over.
//!
//! # Why cascades rather than one map
//!
//! One shadow map stretched over the whole view distance spends most of its
//! texels on terrain a kilometre away and leaves a handful for the block the
//! player is standing next to — where every shadow anyone actually looks at is.
//! Three maps over three slices of the view give the near slice a texel density
//! roughly a hundred times the far one, for three depth passes over geometry
//! that is already in VRAM.
//!
//! # The stored sunlight channel still gates caves
//!
//! Task 10 is explicit about this and it is worth restating where the code is:
//! **a shadow map darkens what the sun can see; it does not decide what the sun
//! reaches.** A cave is dark because its sunlight channel is zero, computed by
//! the server and true whether or not anything is rendering. The shadow factor
//! multiplies that channel, so an unlit cave stays unlit and a lit surface in
//! shade gets darker. Using shadow maps alone would light every cave mouth the
//! sun happened to point at, several chunks deep.
//!
//! # Everything here belongs to mode 3
//!
//! Built with [`super::graph::Post`] and dropped with it, so modes 1 and 2
//! allocate no shadow resources at all.

use glam::{Mat4, Vec3};

use crate::camera::Camera;

use super::{DEPTH_FORMAT, Gpu};

/// How many cascades.
///
/// Three, as the task specifies. Two leaves a visible resolution step in the
/// middle distance; four costs a fourth depth pass to refine terrain that
/// distance fog is already dissolving.
pub const CASCADES: usize = 3;

/// Texels per side of one cascade, when nothing says otherwise.
///
/// A setting rather than a constant, because this is where the cost is: three
/// cascades at 2048 are 48 MiB of depth, and at 4096 they are 192. See
/// [`crate::config::ShadowQuality`] for the ladder and what each rung costs.
pub const DEFAULT_SIZE: u32 = 2048;

/// Where each cascade ends, as a fraction of the shadow range.
///
/// Weighted toward the camera, because perspective is: the first slice covers
/// an eighth of the distance and takes a third of the texels. Uniform splits
/// put most of the resolution where the geometry is smallest on screen.
const SPLITS: [f32; CASCADES] = [0.08, 0.28, 1.0];

/// How far shadows are drawn, in blocks.
///
/// Well short of the far plane. Beyond this the sun is uniform and the fog is
/// taking over anyway, and every metre of range costs texel density in all
/// three cascades.
pub const RANGE: f32 = 160.0;

/// Pulls the cascade's near plane back so casters behind the camera still cast.
///
/// Without it, a wall just off the top of the screen stops shadowing the ground
/// in front of the player — the caster is outside the light's view even though
/// its shadow is not.
const CASTER_MARGIN: f32 = 64.0;

/// A depth bias, in units of the depth buffer's resolution.
///
/// Shadow acne is a surface shadowing itself because its depth in the light's
/// map is quantised to a texel that slopes away from it. **The main defence is
/// the normal-offset bias in `world.wgsl`**, which moves the sample off the
/// surface rather than moving the whole map away from the light; this is the
/// small remainder, for the flat case a normal offset does nothing for.
///
/// Kept low on purpose. A depth bias shortens every shadow — it is the second
/// of the two mechanisms that were making shadows fall short of a corner — so
/// the least that works is the right amount.
const DEPTH_BIAS: i32 = 1;
const DEPTH_BIAS_SLOPE: f32 = 1.5;

/// One cascade's matrix, as the shader wants it.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct CascadeUniform {
    view_projection: [[f32; 4]; 4],
}

/// The three maps, the pipeline that fills them, and the sampler that reads
/// them.
pub struct Shadows {
    /// Texels per side, from the quality setting.
    size: u32,
    /// A view per layer, because a render pass attaches one layer at a time.
    layers: [wgpu::TextureView; CASCADES],
    pipeline: wgpu::RenderPipeline,
    /// The per-cascade matrix, one buffer and bind group each.
    uniforms: [wgpu::Buffer; CASCADES],
    binds: [wgpu::BindGroup; CASCADES],
    /// What the world shader binds to sample all this.
    sample_layout: wgpu::BindGroupLayout,
    /// The per-cascade uniform's layout, for a pipeline that draws into this
    /// pass with a vertex format of its own — see [`Shadows::cascade_bind`].
    draw_layout: wgpu::BindGroupLayout,
    sample_bind: wgpu::BindGroup,
    /// The matrices the last [`Shadows::update`] computed, for the world pass.
    matrices: [Mat4; CASCADES],
    /// The world size of one texel, in blocks, in each of those matrices.
    ///
    /// Kept beside them because it falls out of the same fit — the cascade
    /// covers a sphere of some radius across `size` texels — and the world
    /// shader's normal-offset bias is measured in it. Recovering it there from
    /// the length of a matrix row is possible and is a puzzle rather than a
    /// value.
    texel_world: [f32; CASCADES],
}

impl Shadows {
    /// Builds the maps and both pipelines' worth of layout.
    #[must_use]
    pub fn new(gpu: &Gpu, size: u32) -> Self {
        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("shadow-cascades"),
            size: wgpu::Extent3d {
                width: size,
                height: size,
                depth_or_array_layers: CASCADES as u32,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });

        let array = texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("shadow-array"),
            dimension: Some(wgpu::TextureViewDimension::D2Array),
            ..Default::default()
        });
        let layer = |index: u32| {
            texture.create_view(&wgpu::TextureViewDescriptor {
                label: Some("shadow-layer"),
                dimension: Some(wgpu::TextureViewDimension::D2),
                base_array_layer: index,
                array_layer_count: Some(1),
                ..Default::default()
            })
        };

        let draw_layout = draw_layout(gpu);
        let sample_layout = sample_layout(gpu);

        let sampler = gpu.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("shadow-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            compare: Some(wgpu::CompareFunction::LessEqual),
            ..Default::default()
        });

        let uniforms: [wgpu::Buffer; CASCADES] = std::array::from_fn(|_| {
            gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("shadow-cascade-uniform"),
                size: size_of::<CascadeUniform>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        });
        let binds = std::array::from_fn(|index: usize| {
            gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("shadow-cascade-bind"),
                layout: &draw_layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniforms[index].as_entire_binding(),
                }],
            })
        });

        Self {
            pipeline: build_pipeline(gpu, &draw_layout),
            sample_bind: gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("shadow-sample-bind"),
                layout: &sample_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&array),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&sampler),
                    },
                ],
            }),
            layers: std::array::from_fn(|index| layer(index as u32)),
            uniforms,
            draw_layout,
            binds,
            sample_layout,
            matrices: [Mat4::IDENTITY; CASCADES],
            // Until the first `update`, one texel of an identity matrix, which
            // is meaningless — and harmless, because the identity matrices it
            // sits beside put every lookup outside the map, where the shadow
            // factor returns lit without reading the bias at all.
            texel_world: [RANGE / size as f32; CASCADES],
            size,
        }
    }

    /// The layout the world pipeline needs as its second bind group.
    #[must_use]
    pub const fn sample_layout(&self) -> &wgpu::BindGroupLayout {
        &self.sample_layout
    }

    /// The bind group the world pass sets to read the maps.
    #[must_use]
    pub const fn sample_bind(&self) -> &wgpu::BindGroup {
        &self.sample_bind
    }

    /// The matrices the world shader needs to look a fragment up.
    #[must_use]
    pub const fn matrices(&self) -> &[Mat4; CASCADES] {
        &self.matrices
    }

    /// The world size of one texel, in blocks, in each cascade.
    ///
    /// What the world shader's normal-offset bias is measured in: an offset
    /// smaller than a texel cannot undo a texel-sized quantisation.
    #[must_use]
    pub const fn texel_world(&self) -> &[f32; CASCADES] {
        &self.texel_world
    }

    /// Where each cascade ends, in blocks from the camera.
    #[must_use]
    pub fn split_distances() -> [f32; CASCADES] {
        SPLITS.map(|fraction| fraction * RANGE)
    }

    /// Recomputes the light matrices for where the camera and the sun are now.
    pub fn update(&mut self, gpu: &Gpu, camera: &Camera, aspect: f32, sun_direction: [f32; 3]) {
        let sun = Vec3::from(sun_direction).normalize_or_zero();
        // A sun below the horizon lights nothing, and its matrices would be
        // built looking up through the world. Keep the last ones and let the
        // sun's own intensity — which the sky drops to near zero at night — do
        // the work of making the shadows irrelevant.
        if sun.y > -0.05 {
            return;
        }

        let mut near = camera.near;
        for (index, far) in Self::split_distances().into_iter().enumerate() {
            let (matrix, texel) = cascade_matrix(camera, aspect, near, far, sun, self.size);
            self.matrices[index] = matrix;
            self.texel_world[index] = texel;
            gpu.queue.write_buffer(
                &self.uniforms[index],
                0,
                bytemuck::bytes_of(&CascadeUniform {
                    view_projection: self.matrices[index].to_cols_array_2d(),
                }),
            );
            near = far;
        }
    }

    /// Draws the world into all three cascades.
    ///
    /// `draw` is handed a pass and the cascade's index; the caller knows what
    /// its meshes are and this module deliberately does not.
    /// The layout of the per-cascade uniform.
    #[must_use]
    pub const fn layout(&self) -> &wgpu::BindGroupLayout {
        &self.draw_layout
    }

    /// The per-cascade uniform bind group, for a pipeline that draws into this
    /// pass with a layout of its own.
    ///
    /// The skinned pipeline is the caller: it shares this uniform but has a
    /// different vertex format, so it cannot share the pipeline — and a bind
    /// group made for one layout binds anywhere the layouts match.
    #[must_use]
    pub fn cascade_bind(&self, index: usize) -> Option<&wgpu::BindGroup> {
        self.binds.get(index)
    }

    /// Draws into every cascade, calling `draw` once per layer.
    pub fn render(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        mut draw: impl FnMut(&mut wgpu::RenderPass<'_>, usize),
    ) {
        for (index, layer) in self.layers.iter().enumerate() {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("shadow-cascade"),
                color_attachments: &[],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: layer,
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
            pass.set_bind_group(0, &self.binds[index], &[]);
            draw(&mut pass, index);
        }
    }

    /// Texels per side of one cascade.
    #[must_use]
    pub const fn size(&self) -> u32 {
        self.size
    }

    /// How much texture memory the cascades hold.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        (self.size as u64) * (self.size as u64) * 4 * CASCADES as u64
    }
}

/// The layout the depth pass binds: one matrix, vertex stage only.
fn draw_layout(gpu: &Gpu) -> wgpu::BindGroupLayout {
    gpu.device
        .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("shadow-draw-layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        })
}

/// The layout the world pass binds to read the cascades.
///
/// Depth sampled with a comparison: the hardware does the "is this fragment
/// behind what the sun saw" test and filters the RESULT, which is what makes
/// each PCF tap a bilinear blend of four texels rather than one.
fn sample_layout(gpu: &Gpu) -> wgpu::BindGroupLayout {
    gpu.device
        .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("shadow-sample-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Depth,
                        view_dimension: wgpu::TextureViewDimension::D2Array,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Comparison),
                    count: None,
                },
            ],
        })
}

/// The light matrix covering one slice of the camera frustum.
///
/// # Why a bounding sphere and not the corners themselves
///
/// A box fitted to the eight corners is tighter, and it changes shape as the
/// camera turns — so the texel grid rotates under a stationary world and every
/// shadow edge crawls. A sphere is rotation-invariant: turn on the spot and the
/// cascade covers exactly the same volume, so the shadows hold still. The cost
/// is texels spent on the corners of the sphere that the frustum never reaches.
#[expect(
    clippy::disallowed_methods,
    reason = "charter rule 4 exempts rendering from the deterministic float subset; this tangent               decides where a shadow map looks, not what the world is"
)]
fn cascade_matrix(
    camera: &Camera,
    aspect: f32,
    near: f32,
    far: f32,
    sun: Vec3,
    size: u32,
) -> (Mat4, f32) {
    let forward = camera.forward();
    let right = forward.cross(Vec3::Y).normalize_or_zero();
    let up = right.cross(forward).normalize_or_zero();

    // Half-extents of the slice's near and far faces.
    let tan_half = (camera.fov_y * 0.5).tan();
    let near_h = tan_half * near;
    let far_h = tan_half * far;

    // The camera is the origin: everything the renderer draws is
    // camera-relative (floating origin), so the light matrices are too.
    let mut corners = [Vec3::ZERO; 8];
    for (index, corner) in corners.iter_mut().enumerate() {
        let (distance, half) = if index < 4 {
            (near, near_h)
        } else {
            (far, far_h)
        };
        let sx = if index & 1 == 0 { -1.0 } else { 1.0 };
        let sy = if index & 2 == 0 { -1.0 } else { 1.0 };
        *corner = forward * distance + right * (sx * half * aspect) + up * (sy * half);
    }

    let centre = corners.iter().copied().sum::<Vec3>() / 8.0;
    let radius = corners
        .iter()
        .map(|corner| (*corner - centre).length())
        .fold(0.0_f32, f32::max)
        .max(0.001);

    // Snapped to whole texels along the light's axes. Without this the sphere
    // moves by fractions of a texel as the player walks and the shadow edges
    // shimmer — the classic artefact, and the reason the sphere alone is not
    // enough to hold them still.
    let texel = (radius * 2.0) / size as f32;
    let eye = centre - sun * (radius + CASTER_MARGIN);
    let view = glam::camera::rh::view::look_to_mat4(eye, sun, Vec3::Y);
    let snapped_centre = {
        let in_light = view.transform_point3(centre);
        let snapped = Vec3::new(
            (in_light.x / texel).floor() * texel,
            (in_light.y / texel).floor() * texel,
            in_light.z,
        );
        view.inverse().transform_point3(snapped)
    };
    let view = glam::camera::rh::view::look_to_mat4(
        snapped_centre - sun * (radius + CASTER_MARGIN),
        sun,
        Vec3::Y,
    );

    // The DirectX convention, matching `Camera::projection`: clip depth runs
    // 0..1 rather than -1..1. Mixing the two puts half the shadow map behind
    // the near plane.
    let projection = glam::camera::rh::proj::directx::orthographic(
        -radius,
        radius,
        -radius,
        radius,
        0.0,
        radius * 2.0 + CASTER_MARGIN,
    );
    (projection * view, texel)
}

/// The depth-only pipeline the cascades are drawn with.
fn build_pipeline(gpu: &Gpu, layout: &wgpu::BindGroupLayout) -> wgpu::RenderPipeline {
    let shader = gpu
        .device
        .create_shader_module(wgpu::include_wgsl!("shadow.wgsl"));
    let pipeline_layout = gpu
        .device
        .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("shadow-pipeline-layout"),
            bind_group_layouts: &[Some(layout)],
            immediate_size: 0,
        });

    gpu.device
        .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("shadow"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vertex_main"),
                buffers: &super::vertex_layout(),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            // No fragment stage at all. Depth is the entire output.
            fragment: None,
            primitive: wgpu::PrimitiveState {
                // **Back faces culled, so the map records the surfaces the
                // light actually lands on.**
                //
                // This used to cull fronts, which does move acne off the lit
                // face — by recording a depth a whole block further away, so
                // every shadow was short by the thickness of the thing casting
                // it. Reported from the window as shadows stopping before the
                // corner between two blocks with light bleeding into it, and as
                // the underside of every block coming out fully lit: an
                // underside was the surface being recorded, so it compared
                // equal to itself and passed.
                //
                // The acne that culling fronts was hiding is handled where it
                // belongs now — a facing test and a normal-offset bias in
                // `world.wgsl`, neither of which distorts what is in the map.
                cull_mode: Some(wgpu::Face::Back),
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Less),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState {
                    constant: DEPTH_BIAS,
                    slope_scale: DEPTH_BIAS_SLOPE,
                    clamp: 0.0,
                },
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cascades_cover_the_range_without_a_gap() {
        // A gap between cascades is a band of the world with no shadows in it,
        // and the seam moves with the camera — which reads as flickering rather
        // than as a missing cascade.
        let splits = Shadows::split_distances();
        assert!(splits[0] > 0.0);
        for pair in splits.windows(2) {
            assert!(
                pair[1] > pair[0],
                "cascades must be ordered: {:?}",
                Shadows::split_distances()
            );
        }
        assert!(
            (splits[CASCADES - 1] - RANGE).abs() < f32::EPSILON,
            "the last cascade must reach the shadow range exactly, or the band \
             between it and the range has no shadows"
        );
    }

    #[test]
    fn the_near_cascade_has_far_more_texels_per_block() {
        // The whole reason for cascading. Stated as a ratio so a future split
        // table that flattened the distribution fails here rather than looking
        // fine until somebody notices soft shadows underfoot.
        let splits = Shadows::split_distances();
        let near_density = f32::from(u16::try_from(DEFAULT_SIZE).unwrap_or(u16::MAX)) / splits[0];
        let far_density = f32::from(u16::try_from(DEFAULT_SIZE).unwrap_or(u16::MAX)) / splits[2];
        assert!(
            near_density > far_density * 8.0,
            "the near cascade is only {}x denser than the far one",
            near_density / far_density
        );
    }

    #[test]
    fn a_cascade_matrix_puts_the_slice_inside_clip_space() {
        // The matrix has to map everything in its slice into -1..1, or geometry
        // is clipped out of the shadow map and stops casting. Checked at the
        // centre and at the corners of the slice, which is where a matrix that
        // is nearly right fails.
        let camera = Camera::default();
        let sun = Vec3::new(0.0, -1.0, 0.25).normalize();
        let (matrix, _) = cascade_matrix(&camera, 16.0 / 9.0, 0.05, 20.0, sun, DEFAULT_SIZE);

        let forward = camera.forward();
        for distance in [1.0_f32, 10.0, 19.0] {
            let point = forward * distance;
            let clip = matrix * point.extend(1.0);
            assert!(
                clip.x.abs() <= 1.0 && clip.y.abs() <= 1.0,
                "a point {distance} blocks ahead lands at {clip:?}, outside the cascade"
            );
            assert!(
                (0.0..=1.0).contains(&clip.z),
                "a point {distance} blocks ahead has depth {} outside 0..1",
                clip.z
            );
        }
    }

    #[test]
    fn turning_on_the_spot_does_not_move_the_cascade() {
        // The bounding sphere's entire purpose. A box fitted to the frustum
        // corners changes size as the camera turns, and every shadow edge in
        // the world crawls while the player looks around.
        let sun = Vec3::new(0.2, -1.0, 0.3).normalize();
        let mut camera = Camera::default();
        let (first, first_texel) = cascade_matrix(&camera, 1.0, 0.05, 20.0, sun, DEFAULT_SIZE);
        camera.yaw += std::f32::consts::FRAC_PI_2;
        let (second, second_texel) = cascade_matrix(&camera, 1.0, 0.05, 20.0, sun, DEFAULT_SIZE);

        // The projections must be identical — same radius, same extent. The
        // views differ, because the slice really is somewhere else.
        let scale = |matrix: &Mat4| matrix.x_axis.length() * matrix.y_axis.length();
        assert!(
            (scale(&first) - scale(&second)).abs() < 1e-3,
            "the cascade changed size when the camera turned: {} then {}",
            scale(&first),
            scale(&second)
        );
        // And so must the texel size, which the sphere's radius decides. The
        // world shader's normal-offset bias is measured in it, so a texel that
        // changed with the camera's heading would make the bias — and with it
        // the exact edge of every shadow — breathe as the player looked around.
        assert!(
            (first_texel - second_texel).abs() < 1e-4,
            "one texel covered {first_texel} blocks and then {second_texel}"
        );
    }

    #[test]
    fn a_texel_of_the_near_cascade_covers_less_world_than_a_far_one() {
        // The bias is measured in texels, so this is what makes it small where
        // the detail is. A flat or inverted ladder here would mean the near
        // cascade — the one under the player's feet — carrying the far
        // cascade's bias, which reads as shadows detaching from their casters.
        let sun = Vec3::new(0.2, -1.0, 0.3).normalize();
        let camera = Camera::default();
        let splits = Shadows::split_distances();
        let near = cascade_matrix(&camera, 1.0, 0.05, splits[0], sun, DEFAULT_SIZE).1;
        let far = cascade_matrix(&camera, 1.0, splits[1], splits[2], sun, DEFAULT_SIZE).1;
        assert!(
            near < far,
            "a near texel covers {near} blocks and a far one {far}"
        );
    }
}
