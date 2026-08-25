// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The first-person viewmodel: a hand, and whatever it is holding.
//!
//! # What is the engine's here, and what is a mod's
//!
//! Charter rule 1 puts content in mods, and a hand looks like content. It is
//! not, quite: **a mod cannot express a viewmodel at all**, because there is no
//! way to draw anything attached to the camera. So the mechanism is the
//! engine's — a pass in view space, drawn last, with a swing a mod can trigger —
//! and the same precedent applies as to `engine:humanoid`: the engine ships one
//! so that a client with no mods loaded still has hands.
//!
//! What the hand HOLDS is entirely a mod's, because it is whatever the player
//! is carrying, drawn from the material's own atlas tile.
//!
//! # Boxes, not a rig
//!
//! The skinned rig could pose an arm, and posing one from the camera would mean
//! a first-person pose per clip, per model, forever. A hand is two boxes and a
//! swing; that is what it looks like in the games this is modelled on, and it
//! costs one pipeline and no assets.

use glam::Mat4;

use super::Gpu;

/// One box of the viewmodel, as the shader reads it.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Piece {
    /// Centre in view space, in blocks; `w` is a roll about forward, in radians.
    placement: [f32; 4],
    /// Half-extents in blocks. `w` unused.
    size: [f32; 4],
    /// The atlas rectangle: `u0, v0, u1, v1`. All zero draws untextured.
    uv: [f32; 4],
    /// Multiplied into the result, alpha included.
    tint: [f32; 4],
}

/// Which hand a piece belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hand {
    /// The one that digs and places, on the right.
    Main,
    /// The off-hand, on the left, shown only when it holds something.
    Off,
}

impl Hand {
    /// Which side of the screen it sits on.
    const fn side(self) -> f32 {
        match self {
            Self::Main => 1.0,
            Self::Off => -1.0,
        }
    }
}

/// What the hand is made of, in blocks, in view space.
///
/// Tuned by eye rather than derived: a viewmodel is a composition, and the only
/// test of these numbers is whether a hand looks like it is holding something.
mod shape {
    /// How far from the eye the hand sits.
    pub const DEPTH: f32 = -0.55;
    /// How far to the side.
    ///
    /// **Further out than it was.** Reported from the window: the hand wanted
    /// to be more out of the way, and a viewmodel's job is to say what you are
    /// holding without standing in front of what you are aiming at.
    pub const SIDE: f32 = 0.36;
    /// How far below the middle of the screen.
    pub const DROP: f32 = -0.33;
    /// Half-extents of the forearm.
    pub const ARM: [f32; 3] = [0.055, 0.13, 0.055];
    /// Half-extent of a held block, which is smaller than a real one — a block
    /// at true scale fills a third of the screen.
    pub const BLOCK: f32 = 0.085;
    /// How far the arm tilts inward, in radians, so it points at the middle.
    pub const TILT: f32 = 0.45;
    /// How far up the arm the held thing sits, as a share of the arm's length.
    ///
    /// **Less than one, so it overlaps the hand rather than balancing on it.**
    /// At the arm's full length the block floated off the end of the fingers;
    /// reported from the window, along with the observation that clipping into
    /// the hand is fine and looks better than a gap.
    pub const GRIP: f32 = 0.55;
    /// How far a swing carries the hand, in radians.
    pub const SWING: f32 = 0.9;
    /// How far a swing pulls it back toward the eye, in blocks.
    pub const SWING_DEPTH: f32 = 0.12;
}

/// The colour of a bare hand.
///
/// **The engine's, and deliberately not configurable here.** A mod that wants a
/// different hand supplies a different model, once models can be supplied; a
/// colour picker in the engine would be the engine having an opinion about what
/// a player looks like.
const SKIN: [f32; 4] = [0.85, 0.66, 0.52, 1.0];

/// What one hand is doing this frame.
#[derive(Debug, Clone, Copy, Default)]
pub struct Held {
    /// The atlas rectangle of what it holds, if it holds anything.
    pub tile: Option<[f32; 4]>,
    /// How far through a swing, `0.0..=1.0`.
    pub swing: f32,
}

/// Builds the pieces for one hand.
///
/// Returns nothing for an off-hand holding nothing — an empty left hand hanging
/// in the corner is a thing to explain rather than a thing to see.
#[expect(
    clippy::disallowed_methods,
    reason = "charter rule 4 exempts rendering from the deterministic float subset; these \
              decide where a hand is on the screen, not what the world is"
)]
#[must_use]
pub fn pieces(hand: Hand, held: Held) -> Vec<Piece> {
    if hand == Hand::Off && held.tile.is_none() {
        return Vec::new();
    }
    let side = hand.side();
    // A half-sine, so the hand accelerates out and eases back rather than
    // snapping at both ends.
    let swing = (held.swing.clamp(0.0, 1.0) * std::f32::consts::PI).sin();
    let roll = side * (shape::TILT + swing * shape::SWING);
    let depth = shape::DEPTH + swing * shape::SWING_DEPTH;
    let drop = shape::DROP - swing * 0.06;

    let mut pieces = vec![Piece {
        placement: [side * shape::SIDE, drop, depth, roll],
        size: [shape::ARM[0], shape::ARM[1], shape::ARM[2], 0.0],
        uv: [0.0; 4],
        tint: SKIN,
    }];
    if let Some(tile) = held.tile {
        // Sitting at the end of the arm, which is where a hand is. Offset along
        // the arm's own direction so it follows the swing rather than hanging
        // off it.
        let along = shape::ARM[1] * shape::GRIP + shape::BLOCK * 0.5;
        pieces.push(Piece {
            placement: [
                side * shape::SIDE + roll.sin() * along * side.signum(),
                drop + roll.cos() * along,
                depth,
                roll * 0.4,
            ],
            size: [shape::BLOCK, shape::BLOCK, shape::BLOCK, 0.0],
            uv: tile,
            tint: [1.0, 1.0, 1.0, 1.0],
        });
    }
    pieces
}

/// The viewmodel's pipeline, its uniform, and the pieces for this frame.
pub struct Viewmodel {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    bind: wgpu::BindGroup,
    uniform: wgpu::Buffer,
    instances: wgpu::Buffer,
    capacity: usize,
    drawn: usize,
}

/// How many pieces fit before the buffer grows. Two hands, two boxes each.
const START_CAPACITY: usize = 8;

impl Viewmodel {
    /// Builds the pipeline. `format` is the target it draws into.
    #[must_use]
    pub fn new(
        gpu: &Gpu,
        atlas: &wgpu::TextureView,
        sampler: &wgpu::Sampler,
        format: wgpu::TextureFormat,
    ) -> Self {
        let shader = gpu
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("viewmodel"),
                source: wgpu::ShaderSource::Wgsl(include_str!("viewmodel.wgsl").into()),
            });
        let layout = gpu
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("viewmodel-bind-layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::VERTEX,
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
        let uniform = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("viewmodel-projection"),
            size: size_of::<[[f32; 4]; 4]>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind = make_bind(gpu, &layout, &uniform, atlas, sampler);
        let instances = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("viewmodel-instances"),
            size: (size_of::<Piece>() * START_CAPACITY) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            pipeline: build_pipeline(gpu, &shader, &layout, format),
            layout,
            bind,
            uniform,
            instances,
            capacity: START_CAPACITY,
            drawn: 0,
        }
    }

    /// Points it at a new atlas, when the material table arrives.
    pub fn set_atlas(&mut self, gpu: &Gpu, atlas: &wgpu::TextureView, sampler: &wgpu::Sampler) {
        self.bind = make_bind(gpu, &self.layout, &self.uniform, atlas, sampler);
    }

    /// Uploads this frame's pieces and the projection they are drawn with.
    pub fn prepare(&mut self, gpu: &Gpu, projection: Mat4, pieces: &[Piece]) {
        self.drawn = pieces.len();
        gpu.queue.write_buffer(
            &self.uniform,
            0,
            bytemuck::bytes_of(&projection.to_cols_array_2d()),
        );
        if pieces.is_empty() {
            return;
        }
        if pieces.len() > self.capacity {
            let capacity = pieces.len().next_power_of_two();
            self.instances = gpu.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("viewmodel-instances"),
                size: (size_of::<Piece>() * capacity) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.capacity = capacity;
        }
        gpu.queue
            .write_buffer(&self.instances, 0, bytemuck::cast_slice(pieces));
    }

    /// Draws it. Nothing happens when there is nothing in hand.
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        if self.drawn == 0 {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind, &[]);
        pass.set_vertex_buffer(0, self.instances.slice(..));
        let count = u32::try_from(self.drawn).unwrap_or(0);
        // 36 vertices is one cube, built in the vertex stage.
        pass.draw(0..36, 0..count);
    }
}

fn make_bind(
    gpu: &Gpu,
    layout: &wgpu::BindGroupLayout,
    uniform: &wgpu::Buffer,
    atlas: &wgpu::TextureView,
    sampler: &wgpu::Sampler,
) -> wgpu::BindGroup {
    gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("viewmodel-bind"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform.as_entire_binding(),
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

fn build_pipeline(
    gpu: &Gpu,
    shader: &wgpu::ShaderModule,
    layout: &wgpu::BindGroupLayout,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    let pipeline_layout = gpu
        .device
        .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("viewmodel-pipeline-layout"),
            bind_group_layouts: &[Some(layout)],
            immediate_size: 0,
        });
    gpu.device
        .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("viewmodel"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: shader,
                entry_point: Some("vertex_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: size_of::<Piece>() as u64,
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
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x4,
                            offset: (size_of::<[f32; 4]>() * 2) as u64,
                            shader_location: 2,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x4,
                            offset: (size_of::<[f32; 4]>() * 3) as u64,
                            shader_location: 3,
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
                cull_mode: Some(wgpu::Face::Back),
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            // **No depth at all**, which is the point — see the shader. A hand
            // is nearer than everything by construction, and depth-testing it
            // would hide it whenever the player stood near a wall.
            depth_stencil: None,

            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_off_hand_draws_nothing_at_all() {
        // A bare left hand hanging in the corner is a thing to explain rather
        // than a thing to see. The main hand is always there, empty or not.
        assert!(pieces(Hand::Off, Held::default()).is_empty());
        assert_eq!(pieces(Hand::Main, Held::default()).len(), 1);
    }

    #[test]
    fn a_held_block_is_a_second_box_at_the_end_of_the_arm() {
        let held = Held {
            tile: Some([0.1, 0.1, 0.2, 0.2]),
            swing: 0.0,
        };
        let main = pieces(Hand::Main, held);
        assert_eq!(main.len(), 2, "an arm and the thing it is holding");
        assert!(
            main[0].uv.iter().all(|edge| *edge == 0.0),
            "the arm is not textured"
        );
        let tile = held.tile.expect("set");
        assert!(
            main[1]
                .uv
                .iter()
                .zip(tile)
                .all(|(drawn, wanted)| (drawn - wanted).abs() < 1e-6),
            "the block does not sample the tile it was given"
        );
        // The block is further from the eye's level than the arm's centre:
        // it sits at the far end of it, not inside it.
        assert!(
            main[1].placement[1] > main[0].placement[1],
            "the block is not at the end of the arm: {:?} against {:?}",
            main[1].placement,
            main[0].placement
        );
        // And the off-hand is on the other side of the screen.
        let off = pieces(Hand::Off, held);
        assert!(
            off[0].placement[0] < 0.0 && main[0].placement[0] > 0.0,
            "both hands are on the same side"
        );
    }

    #[test]
    fn a_swing_moves_the_hand_and_comes_back() {
        let held = Held {
            tile: None,
            swing: 0.0,
        };
        let rest = pieces(Hand::Main, held)[0].placement;
        let mid = pieces(Hand::Main, Held { swing: 0.5, ..held })[0].placement;
        let end = pieces(Hand::Main, Held { swing: 1.0, ..held })[0].placement;

        assert!(
            (mid[3] - rest[3]).abs() > 0.1,
            "the hand does not move through the swing"
        );
        // **Back where it started**, or a dig would leave the hand somewhere
        // new every time and it would walk off the screen.
        for axis in 0..4 {
            assert!(
                (end[axis] - rest[axis]).abs() < 1e-5,
                "axis {axis} ended at {} and started at {}",
                end[axis],
                rest[axis]
            );
        }
    }

    #[test]
    fn a_swing_outside_its_range_is_clamped_rather_than_wrapping() {
        // The phase comes from a timer, and a timer that overran would send the
        // hand round in a circle.
        let far = pieces(
            Hand::Main,
            Held {
                tile: None,
                swing: 7.5,
            },
        )[0]
        .placement;
        let end = pieces(
            Hand::Main,
            Held {
                tile: None,
                swing: 1.0,
            },
        )[0]
        .placement;
        assert!((far[3] - end[3]).abs() < 1e-5);
    }
}
