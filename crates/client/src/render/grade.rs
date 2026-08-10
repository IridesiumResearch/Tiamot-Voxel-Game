// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Time-of-day colour grading, as a 3D lookup table.
//!
//! # What a mod says, and what happens to it
//!
//! A sky keyframe carries a [`SkyGrade`](tiamot_core::proto::SkyGrade): exposure,
//! tint, offset, contrast, saturation, gamma. The client interpolates those six
//! between keyframes (`crate::sky`), bakes the result into a small 3D texture
//! here, and the composite pass looks every finished pixel up in it. Exposure is
//! the exception and does not enter the table — it multiplies the scene *before*
//! the highlight roll-off, because its whole job is to decide how much of the
//! picture reaches the shoulder.
//!
//! # Why a table rather than the six lines of arithmetic it bakes
//!
//! The arithmetic is cheaper per pixel than a texture fetch, so this is not a
//! performance win today and it would be dishonest to present it as one. Two
//! things buy it:
//!
//! - **The per-pixel cost stops depending on how complicated grading gets.**
//!   Whatever a grade grows into — a filmic curve, split toning, a channel
//!   mixer — the composite still does one trilinear fetch.
//! - **It is the seam a mod-supplied table plugs into.** A `.cube` file pushed
//!   by a server is a different way to fill exactly this texture, and needs no
//!   shader change. The parametric grade is the first filler, not the only
//!   possible one.
//!
//! # Why the table is sRGB-encoded
//!
//! It stores *display-referred* values in 8 bits. Stored linearly, 8 bits puts
//! its quantisation steps in the wrong places: evenly spread through a range the
//! eye reads logarithmically, which is visible banding in the darks — and night
//! is exactly when a grade is doing the most work. Encoding sRGB and letting the
//! sampler decode spends the same 8 bits where the eye is looking.
//!
//! # The identity is skipped, not baked
//!
//! An 8-bit table of the identity is not quite the identity. Ungraded worlds —
//! which is every world that predates this module, and every world whose mods
//! register no sky — must be untouched rather than nearly untouched, because
//! Task 08's screenshot hashes are asserted on exact values. So the composite
//! carries a flag and grades nothing when the grade is [`SkyGrade::NONE`].

use tiamot_core::proto::SkyGrade;

/// Samples per axis in the table.
///
/// Sixteen, giving 4,096 entries. The grade this bakes is smooth — a product of
/// multiplies, a lerp and a power — so trilinear interpolation between samples
/// that coarse is accurate to well under a quantisation step. A 33³ table, the
/// film convention, exists because film LUTs encode hand-drawn curves with kinks
/// in them; nothing here has a kink.
pub const SIZE: u32 = 16;

/// Bytes one baked table occupies. Four channels, eight bits each.
pub const BYTES: u64 = (SIZE as u64) * (SIZE as u64) * (SIZE as u64) * 4;

/// Mid grey, the pivot contrast turns about.
///
/// Contrast has to push away from *something*, and pushing away from zero is
/// just gain. Half is the conventional choice and it is the one the keyframes in
/// `game/core_sky` are written against.
const PIVOT: f32 = 0.5;

/// A baked table and the grade it came from.
///
/// Holds the grade so a frame can ask whether the table it already has is the
/// one it needs — the interpolated grade moves every frame by an amount too
/// small to see, and re-baking 4,096 entries for that would be a per-frame cost
/// for no per-frame difference.
pub struct Grading {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    /// What the current contents were baked from, or `None` when nothing has
    /// been baked yet.
    baked: Option<SkyGrade>,
}

impl Grading {
    /// Allocates the table. Contents are undefined until [`Grading::bake`].
    #[must_use]
    pub fn new(gpu: &super::Gpu) -> Self {
        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("grade-lut"),
            size: wgpu::Extent3d {
                width: SIZE,
                height: SIZE,
                depth_or_array_layers: SIZE,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D3,
            // sRGB, so the sampler decodes and the eight bits land where the eye
            // is looking. See the module docs.
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        Self {
            view: texture.create_view(&wgpu::TextureViewDescriptor::default()),
            texture,
            baked: None,
        }
    }

    /// The view the composite samples.
    #[must_use]
    pub const fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    /// Re-bakes the table if `grade` has moved far enough to matter.
    ///
    /// Returns whether anything was uploaded, for the benchmark and the test
    /// that a still sky does not re-bake every frame.
    pub fn bake(&mut self, gpu: &super::Gpu, grade: &SkyGrade) -> bool {
        if self
            .baked
            .is_some_and(|baked| !moved_visibly(&baked, grade))
        {
            return false;
        }

        let table = table(grade);
        gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &table,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(SIZE * 4),
                rows_per_image: Some(SIZE),
            },
            wgpu::Extent3d {
                width: SIZE,
                height: SIZE,
                depth_or_array_layers: SIZE,
            },
        );
        self.baked = Some(*grade);
        true
    }
}

/// Whether two grades differ by enough for the difference to survive to a pixel.
///
/// The threshold is half a step of the eight bits the table is stored in. Below
/// that the two tables would be byte-identical, so re-baking produces the same
/// upload twice.
fn moved_visibly(from: &SkyGrade, to: &SkyGrade) -> bool {
    /// Half of `1/255`.
    const EPSILON: f32 = 1.0 / 510.0;

    let scalars = [
        (from.exposure, to.exposure),
        (from.contrast, to.contrast),
        (from.saturation, to.saturation),
        (from.gamma, to.gamma),
    ];
    scalars.iter().any(|(from, to)| (from - to).abs() > EPSILON)
        || (0..3).any(|channel| {
            (from.tint[channel] - to.tint[channel]).abs() > EPSILON
                || (from.offset[channel] - to.offset[channel]).abs() > EPSILON
        })
}

/// Bakes the table: `SIZE³` RGBA8 entries, sRGB-encoded, in the layout
/// `write_texture` wants — x fastest, then y, then z.
///
/// **Which axis is which matters.** The shader looks a colour up at
/// `(r, g, b)`, so x has to be red, y green and z blue; getting that wrong
/// swaps channels in a way that looks like a grading bug rather than an indexing
/// one.
#[must_use]
fn table(grade: &SkyGrade) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(BYTES as usize);
    let last = (SIZE - 1) as f32;
    for blue in 0..SIZE {
        for green in 0..SIZE {
            for red in 0..SIZE {
                let input = [red as f32 / last, green as f32 / last, blue as f32 / last];
                let graded = apply(grade, input);
                for channel in graded {
                    bytes.push(encode_srgb(channel));
                }
                // Opaque. The composite ignores alpha, but a zero here would
                // make the table unreadable in a debugger and in any tool that
                // previews it.
                bytes.push(u8::MAX);
            }
        }
    }
    bytes
}

/// The grade itself: what one display-referred colour becomes.
///
/// The order is fixed and documented on
/// [`SkyGrade`](tiamot_core::proto::SkyGrade): contrast about [`PIVOT`],
/// saturation towards luma, `tint` then `offset`, and `gamma` last. Exposure is
/// not here — it applies before the tonemap, upstream of anything this sees.
#[expect(
    clippy::disallowed_methods,
    reason = "charter rule 4 exempts rendering from the deterministic float subset; a grading \
              table is built on the machine that displays it and never reaches the tick or the \
              hash gate"
)]
#[must_use]
pub fn apply(grade: &SkyGrade, colour: [f32; 3]) -> [f32; 3] {
    let mut out = colour;

    for channel in &mut out {
        *channel = (*channel - PIVOT) * grade.contrast + PIVOT;
    }

    // Rec. 709 luma, the same weights the bloom threshold uses. A saturation of
    // zero lands on grey of the same brightness rather than on the channel
    // average, which is a different and duller grey.
    let luma = out[0] * 0.2126 + out[1] * 0.7152 + out[2] * 0.0722;
    for channel in &mut out {
        *channel = luma + (*channel - luma) * grade.saturation;
    }

    for (channel, (tint, offset)) in out.iter_mut().zip(grade.tint.iter().zip(&grade.offset)) {
        *channel = *channel * tint + offset;
    }

    // Clamped before the power, not after: a negative channel — which `offset`
    // can produce, and contrast can too — raised to a fractional power is NaN,
    // and a NaN here is a black pixel that moves when the sun does.
    for channel in &mut out {
        *channel = clamp_unit(*channel).powf(1.0 / grade.gamma);
    }

    out
}

/// Clamps to `0..=1`, mapping a non-finite value to zero rather than letting it
/// through.
fn clamp_unit(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// Encodes a unit value as an sRGB byte.
///
/// The real piecewise transfer function rather than a gamma-2.2 approximation.
/// The linear segment near black is the part that matters here — approximating
/// it is what puts a visible step in a night sky.
#[expect(
    clippy::disallowed_methods,
    reason = "charter rule 4 exempts rendering; see `apply`"
)]
fn encode_srgb(linear: f32) -> u8 {
    let linear = clamp_unit(linear);
    let encoded = if linear <= 0.003_130_8 {
        linear * 12.92
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    };
    // `+ 0.5` to round rather than truncate: truncating biases the whole table
    // dark by half a step, which is a global brightness error in a function
    // whose identity case has to be as close to exact as eight bits allow.
    (encoded * 255.0 + 0.5) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The identity grade must leave a colour exactly alone.
    ///
    /// Not a formality: every knob is applied unconditionally, so an off-by-one
    /// in the pivot or a `1.0 / gamma` written as `gamma` shows up here and
    /// nowhere else until someone looks at a screenshot.
    #[test]
    fn the_identity_grade_changes_nothing() {
        for step in 0..=16 {
            let value = step as f32 / 16.0;
            let out = apply(&SkyGrade::NONE, [value, value, value]);
            for channel in out {
                assert!(
                    (channel - value).abs() < 1e-6,
                    "{value} came back as {channel}"
                );
            }
        }
    }

    #[test]
    fn saturation_of_zero_lands_on_grey_of_the_same_brightness() {
        let grade = SkyGrade {
            saturation: 0.0,
            ..SkyGrade::NONE
        };
        let out = apply(&grade, [0.8, 0.2, 0.4]);
        assert!(
            (out[0] - out[1]).abs() < 1e-6 && (out[1] - out[2]).abs() < 1e-6,
            "not grey: {out:?}"
        );
        // And the grey it lands on is the luma, not the mean of the channels —
        // the mean would be 0.467 and read as a duller picture overall.
        let luma = 0.8 * 0.2126 + 0.2 * 0.7152 + 0.4 * 0.0722;
        assert!((out[0] - luma).abs() < 1e-6, "{out:?} against luma {luma}");
    }

    #[test]
    fn contrast_turns_about_mid_grey_and_leaves_it_alone() {
        let grade = SkyGrade {
            contrast: 2.0,
            ..SkyGrade::NONE
        };
        let pivot = apply(&grade, [PIVOT; 3]);
        assert!(
            (pivot[0] - PIVOT).abs() < 1e-6,
            "the pivot moved: {pivot:?}"
        );
        // Above the pivot goes up, below goes down.
        assert!(apply(&grade, [0.6; 3])[0] > 0.6);
        assert!(apply(&grade, [0.4; 3])[0] < 0.4);
    }

    #[test]
    fn a_grade_that_would_go_negative_clamps_rather_than_producing_nan() {
        // `offset` can take a channel below zero, and a negative number raised
        // to a fractional power is NaN — which reaches the screen as a black
        // pixel that moves with the sun.
        let grade = SkyGrade {
            offset: [-1.0; 3],
            gamma: 2.2,
            ..SkyGrade::NONE
        };
        let out = apply(&grade, [0.2; 3]);
        for channel in out {
            assert!(channel.is_finite(), "{out:?}");
            assert!((0.0..=1.0).contains(&channel), "{out:?}");
        }
    }

    #[test]
    fn the_table_is_the_size_the_texture_expects() {
        assert_eq!(table(&SkyGrade::NONE).len() as u64, BYTES);
    }

    /// The axis order the shader assumes, asserted on the bytes.
    ///
    /// A table with red and blue swapped is still a plausible-looking table —
    /// it grades, it just grades the wrong channel — so the only way to catch
    /// the mistake is to read the layout back.
    #[test]
    fn red_runs_along_x_and_blue_along_z() {
        let table = table(&SkyGrade::NONE);
        let at = |red: u32, green: u32, blue: u32| -> [u8; 3] {
            let index = ((blue * SIZE * SIZE + green * SIZE + red) * 4) as usize;
            [table[index], table[index + 1], table[index + 2]]
        };

        // The far end of x is full red and nothing else.
        let red = at(SIZE - 1, 0, 0);
        assert_eq!(red[0], u8::MAX, "x should be red: {red:?}");
        assert_eq!((red[1], red[2]), (0, 0), "x should be red alone: {red:?}");

        // And the far end of z is full blue.
        let blue = at(0, 0, SIZE - 1);
        assert_eq!(blue[2], u8::MAX, "z should be blue: {blue:?}");
        assert_eq!(
            (blue[0], blue[1]),
            (0, 0),
            "z should be blue alone: {blue:?}"
        );
    }

    /// The identity table must round-trip through eight sRGB bits closely
    /// enough that grading an ungraded frame would be invisible — which is what
    /// justifies the skip being an optimisation rather than a correctness fix.
    #[test]
    fn the_identity_table_survives_eight_bits() {
        let table = table(&SkyGrade::NONE);
        let last = (SIZE - 1) as f32;
        for step in 0..SIZE {
            let index = ((step * SIZE * SIZE + step * SIZE + step) * 4) as usize;
            let expected = encode_srgb(step as f32 / last);
            assert_eq!(
                table[index], expected,
                "the diagonal is not the identity at {step}"
            );
        }
    }

    #[test]
    fn srgb_encoding_hits_the_ends_exactly() {
        assert_eq!(encode_srgb(0.0), 0);
        assert_eq!(encode_srgb(1.0), 255);
        // And mid grey lands where sRGB says it does: linear 0.5 is 188.
        assert_eq!(encode_srgb(0.5), 188);
    }

    #[test]
    fn a_grade_that_has_not_moved_does_not_count_as_moved() {
        let grade = SkyGrade::NONE;
        assert!(!moved_visibly(&grade, &grade));
        // A change under half a quantisation step cannot reach a pixel.
        let nudged = SkyGrade {
            contrast: 1.0 + 1.0 / 4096.0,
            ..grade
        };
        assert!(!moved_visibly(&grade, &nudged));
        // One over it can.
        let moved = SkyGrade {
            contrast: 1.1,
            ..grade
        };
        assert!(moved_visibly(&grade, &moved));
    }
}
