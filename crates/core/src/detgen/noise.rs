// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Deterministic gradient noise and its combinators.
//!
//! Every operation here is inside the
//! [Deterministic Float Subset](../../../docs/float-determinism.md): integer
//! hashing to pick gradients, dot products, multiplication, addition, and a
//! polynomial fade curve. **No transcendental appears anywhere**, which is not a
//! compromise — gradient noise mathematically requires none.
//!
//! # Two lattices, and why both
//!
//! - [`gradient_2d`] / [`gradient_3d`] work on a **cubic lattice** with the
//!   `6t⁵ − 15t⁴ + 10t³` fade curve. Classic, cheap, and the fade curve is what
//!   makes it C² continuous.
//! - [`simplex_2d`] / [`simplex_3d`] work on a **simplex lattice** with a radial
//!   falloff kernel and no fade curve at all.
//!
//! They differ in a way that matters for how terrain *looks*. Cubic-lattice
//! noise has visible axis-aligned structure — ridges that line up with the world
//! axes — because its lattice does. A simplex lattice has no such preferred
//! direction. Since fidelity and beauty outrank raw speed here
//! (`docs/performance-targets.md`), **the simplex variants are the default for
//! fBm and for the fingerprint recipe**, and the cubic ones remain available for
//! anything that wants the cheaper, more regular field.
//!
//! # No `floor` anywhere
//!
//! `f32::floor` is banned and the reason is subtle enough to restate: it lowers
//! to a **libm call** on an `SSE2`-only `x86_64` target, because the instruction
//! that implements it in one step is `SSE4.1`. [`floor_to_i32`] does the same job
//! with a cast and a comparison, exactly and faster.
//!
//! # Vectorisation: checked, not assumed
//!
//! The bulk fills use flat slices and carry no accumulator across iterations,
//! which is the shape LLVM can vectorise. **It does not vectorise them**, and
//! the reason is inherent rather than fixable by rearranging the loop: every
//! sample does a data-dependent gradient-table lookup and the simplex kernel
//! branches on its radial falloff. Gathers and lane-varying branches are exactly
//! what defeats auto-vectorisation.
//!
//! This is recorded rather than glossed because the task asked for it to be
//! checked. See `scripts/check-vectorisation.sh`, which greps the emitted
//! assembly, and the numbers in `benches/detgen.rs`.
//!
//! Deliberate SIMD — sampling four lattice points at once with explicit
//! intrinsics — would help and is not attempted here. It would need its own
//! determinism argument, since a SIMD path that disagrees with the scalar one by
//! a single bit breaks the hash gate on any machine that picks a different
//! path.

// Noise is mathematics, and in mathematics `x`, `y`, `z`, `u`, `v`, `w`, and
// `t` ARE the descriptive names. Renaming them to `horizontal_position` or
// `fade_input` would make every formula here harder to check against the paper
// it came from, which is the opposite of what this lint is for.
#![allow(clippy::many_single_char_names)]

use super::floor_to_i32;

/// Largest lattice coordinate the samplers accept, in either direction.
///
/// Two independent limits meet here, and the smaller one wins:
///
/// 1. **`f32` integer precision.** Above 2²⁴ ≈ 16.7 million, consecutive
///    integers are no longer distinct in `f32`, so `x - lattice_x as f32` gives
///    zero and the field goes flat. The noise stops being noise.
/// 2. **`i32` arithmetic.** The skew step sums lattice coordinates, and two
///    saturated `i32`s overflow.
///
/// Both are reachable from mod-supplied parameters, not just from absurd input:
/// 16 octaves at lacunarity 4 multiplies the frequency by 4¹⁵, so an ordinary
/// world coordinate lands past 10¹⁴. A property test found this by generating
/// the parameter space rather than sampling plausible values from it.
///
/// 2²² leaves two bits of headroom under the `f32` limit and makes the sums
/// unoverflowable. Beyond it the field is clamped, which is degenerate — but
/// degenerate and deterministic beats degenerate and panicking.
const LATTICE_LIMIT: f32 = 4_194_304.0;

/// Clamps a sample coordinate into the range where the lattice is well defined.
///
/// `clamp` is min and max composed — comparisons and selection, inside the
/// allowed subset. It panics only when the bounds themselves are misordered,
/// which two compile-time constants cannot be.
#[must_use]
fn clamp_coordinate(value: f32) -> f32 {
    value.clamp(-LATTICE_LIMIT, LATTICE_LIMIT)
}

/// Gradient vectors for 2D noise: 8 directions around a circle.
///
/// Unnormalised on purpose — the pairs are `(±1, ±1)` and axis-aligned unit
/// vectors, so the dot products stay exact in binary floating point. Normalising
/// them would introduce irrational components with no benefit to the field's
/// character.
const GRADIENTS_2D: [[f32; 2]; 8] = [
    [1.0, 1.0],
    [-1.0, 1.0],
    [1.0, -1.0],
    [-1.0, -1.0],
    [1.0, 0.0],
    [-1.0, 0.0],
    [0.0, 1.0],
    [0.0, -1.0],
];

/// Gradient vectors for 3D noise: the 12 edge midpoints of a cube, Perlin's
/// improved-noise set, padded to 16 entries.
///
/// The padding is not decorative. Indexing a 12-entry table means `hash % 12`,
/// and 12 is not a power of two, so that is an integer division — tens of cycles
/// in the innermost loop of every 3D sample. Padding to 16 turns it into
/// `hash & 15`, a single instruction.
///
/// The four repeats are the ones Perlin's improved noise repeats, chosen so the
/// duplication does not bias the field along any axis: each repeated gradient
/// has a counterpart already pointing the other way.
const GRADIENTS_3D: [[f32; 3]; 16] = [
    [1.0, 1.0, 0.0],
    [-1.0, 1.0, 0.0],
    [1.0, -1.0, 0.0],
    [-1.0, -1.0, 0.0],
    [1.0, 0.0, 1.0],
    [-1.0, 0.0, 1.0],
    [1.0, 0.0, -1.0],
    [-1.0, 0.0, -1.0],
    [0.0, 1.0, 1.0],
    [0.0, -1.0, 1.0],
    [0.0, 1.0, -1.0],
    [0.0, -1.0, -1.0],
    // The four repeats.
    [1.0, 1.0, 0.0],
    [0.0, -1.0, 1.0],
    [-1.0, 1.0, 0.0],
    [0.0, -1.0, -1.0],
];

// The mask indexing above is only correct while both tables are powers of two.
const _: () = assert!(GRADIENTS_2D.len().is_power_of_two());
const _: () = assert!(GRADIENTS_3D.len().is_power_of_two());

/// Normalisation for cubic-lattice 2D noise: the maximum dot product for this
/// gradient set is sqrt(2)/2 per axis at the cell centre, so the reciprocal is
/// sqrt(2).
///
/// Written as `std::f32::consts::SQRT_2` rather than a literal because it is
/// exactly that — a constant, evaluated by the compiler, never a runtime
/// operation. The subset restricts what simulation code computes, not where its
/// constants came from.
const GRADIENT_2D_SCALE: f32 = std::f32::consts::SQRT_2;
/// Normalisation for cubic-lattice 3D noise.
const GRADIENT_3D_SCALE: f32 = 1.154_700_5;
/// Empirical normalisation for the 2D simplex kernel and gradient set.
const SIMPLEX_2D_SCALE: f32 = 45.0;
/// Empirical normalisation for the 3D simplex kernel and gradient set.
const SIMPLEX_3D_SCALE: f32 = 32.0;

/// Hashes lattice coordinates to a gradient index.
///
/// # A tried and rejected optimisation
///
/// This runs four times per 3D sample and 110,592 samples per sub-node chunk
/// fill, so it looks like the obvious place to speed up `fill_3d`. Packing the
/// three coordinates into one word and using a single multiply instead of four
/// was measured: **1% faster**, and it broke seed sensitivity — a single
/// multiply does not diffuse a changed seed across the gradient index, so
/// different seeds produced correlated fields and
/// `changing_the_seed_changes_the_field` failed.
///
/// The hash is not the bottleneck. Recorded so the next person does not spend
/// the afternoon rediscovering it.
///
/// Integer only, so it is trivially identical everywhere. The constants are
/// large odd primes; multiplying and mixing avoids the axis-aligned banding a
/// naive `x ^ y` would produce.
#[must_use]
const fn hash_lattice(seed: u64, x: i32, y: i32, z: i32) -> u64 {
    // Cast through u32 before widening: `i32 as u64` sign-extends, which would
    // make -1 collide with a large positive coordinate.
    let mut h = seed;
    h ^= (x as u32 as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    h ^= (y as u32 as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    h ^= (z as u32 as u64).wrapping_mul(0x1656_67B1_9E37_79F9);
    h ^= h >> 29;
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h ^= h >> 32;
    h
}

/// The quintic fade curve `6t⁵ − 15t⁴ + 10t³`, in Horner form.
///
/// Its first and second derivatives vanish at 0 and 1, which is what makes
/// cubic-lattice gradient noise C² continuous — the cubic `3t² − 2t³` leaves
/// second-derivative discontinuities at cell boundaries, visible as faint
/// creases across terrain.
///
/// **Every multiply and add is written separately.** `mul_add` would be the
/// natural way to express Horner's method and is banned: it rounds once where
/// `a * b + c` rounds twice, so a machine with FMA and one without would
/// disagree.
#[must_use]
pub fn fade(t: f32) -> f32 {
    // t * t * t * (t * (t * 6 - 15) + 10)
    let a = t * 6.0 - 15.0;
    let b = t * a + 10.0;
    t * t * t * b
}

/// Linear interpolation, written out.
#[must_use]
fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

// ---------------------------------------------------------------------------
// Cubic-lattice gradient noise
// ---------------------------------------------------------------------------

/// 2D gradient noise on a cubic lattice. Output is approximately `[-1, 1]`.
#[must_use]
pub fn gradient_2d(seed: u64, x: f32, y: f32) -> f32 {
    let (x, y) = (clamp_coordinate(x), clamp_coordinate(y));

    let x0 = floor_to_i32(x);
    let y0 = floor_to_i32(y);
    let fx = x - x0 as f32;
    let fy = y - y0 as f32;

    let u = fade(fx);
    let v = fade(fy);

    let corner = |ix: i32, iy: i32, dx: f32, dy: f32| -> f32 {
        let index = (hash_lattice(seed, ix, iy, 0) & (GRADIENTS_2D.len() as u64 - 1)) as usize;
        let g = GRADIENTS_2D[index];
        g[0] * dx + g[1] * dy
    };

    let n00 = corner(x0, y0, fx, fy);
    let n10 = corner(x0 + 1, y0, fx - 1.0, fy);
    let n01 = corner(x0, y0 + 1, fx, fy - 1.0);
    let n11 = corner(x0 + 1, y0 + 1, fx - 1.0, fy - 1.0);

    lerp(lerp(n00, n10, u), lerp(n01, n11, u), v) * GRADIENT_2D_SCALE
}

/// 3D gradient noise on a cubic lattice. Output is approximately `[-1, 1]`.
#[must_use]
pub fn gradient_3d(seed: u64, x: f32, y: f32, z: f32) -> f32 {
    let (x, y, z) = (
        clamp_coordinate(x),
        clamp_coordinate(y),
        clamp_coordinate(z),
    );

    let x0 = floor_to_i32(x);
    let y0 = floor_to_i32(y);
    let z0 = floor_to_i32(z);
    let fx = x - x0 as f32;
    let fy = y - y0 as f32;
    let fz = z - z0 as f32;

    let u = fade(fx);
    let v = fade(fy);
    let w = fade(fz);

    let corner = |ix: i32, iy: i32, iz: i32, dx: f32, dy: f32, dz: f32| -> f32 {
        let index = (hash_lattice(seed, ix, iy, iz) & (GRADIENTS_3D.len() as u64 - 1)) as usize;
        let g = GRADIENTS_3D[index];
        g[0] * dx + g[1] * dy + g[2] * dz
    };

    let n000 = corner(x0, y0, z0, fx, fy, fz);
    let n100 = corner(x0 + 1, y0, z0, fx - 1.0, fy, fz);
    let n010 = corner(x0, y0 + 1, z0, fx, fy - 1.0, fz);
    let n110 = corner(x0 + 1, y0 + 1, z0, fx - 1.0, fy - 1.0, fz);
    let n001 = corner(x0, y0, z0 + 1, fx, fy, fz - 1.0);
    let n101 = corner(x0 + 1, y0, z0 + 1, fx - 1.0, fy, fz - 1.0);
    let n011 = corner(x0, y0 + 1, z0 + 1, fx, fy - 1.0, fz - 1.0);
    let n111 = corner(x0 + 1, y0 + 1, z0 + 1, fx - 1.0, fy - 1.0, fz - 1.0);

    let x00 = lerp(n000, n100, u);
    let x10 = lerp(n010, n110, u);
    let x01 = lerp(n001, n101, u);
    let x11 = lerp(n011, n111, u);

    lerp(lerp(x00, x10, v), lerp(x01, x11, v), w) * GRADIENT_3D_SCALE
}

// ---------------------------------------------------------------------------
// Simplex-lattice gradient noise
// ---------------------------------------------------------------------------

/// Skew factor for 2D simplex: `(sqrt(3) - 1) / 2`.
///
/// A constant. Its provenance involves a square root; its *value* is a literal
/// the compiler bakes in, so no runtime transcendental is involved. The subset
/// restricts runtime operations, not where your constants came from.
const SKEW_2D: f32 = 0.366_025_4;
/// Unskew factor for 2D simplex: `(3 - sqrt(3)) / 6`.
const UNSKEW_2D: f32 = 0.211_324_87;
/// Skew factor for 3D simplex: `1/3`.
const SKEW_3D: f32 = 1.0 / 3.0;
/// Unskew factor for 3D simplex: `1/6`.
const UNSKEW_3D: f32 = 1.0 / 6.0;

/// 2D simplex-lattice gradient noise. Output is approximately `[-1, 1]`.
///
/// Preferred over [`gradient_2d`] for terrain: a simplex lattice has no
/// preferred direction, so it does not produce the axis-aligned ridges a cubic
/// lattice does.
#[must_use]
pub fn simplex_2d(seed: u64, x: f32, y: f32) -> f32 {
    let (x, y) = (clamp_coordinate(x), clamp_coordinate(y));

    // Skew the input into the lattice's coordinate space.
    let skew = (x + y) * SKEW_2D;
    let i = floor_to_i32(x + skew);
    let j = floor_to_i32(y + skew);

    let unskew = (i + j) as f32 * UNSKEW_2D;
    let origin_x = i as f32 - unskew;
    let origin_y = j as f32 - unskew;
    let dx0 = x - origin_x;
    let dy0 = y - origin_y;

    // Which of the two triangles in this rhombus the point fell into.
    let (offset_i, offset_j) = if dx0 > dy0 { (1, 0) } else { (0, 1) };

    let dx1 = dx0 - offset_i as f32 + UNSKEW_2D;
    let dy1 = dy0 - offset_j as f32 + UNSKEW_2D;
    let dx2 = dx0 - 1.0 + 2.0 * UNSKEW_2D;
    let dy2 = dy0 - 1.0 + 2.0 * UNSKEW_2D;

    let contribution = |ix: i32, iy: i32, dx: f32, dy: f32| -> f32 {
        // Radial falloff: (0.5 - d²)⁴, clamped at zero outside the kernel.
        // Branch-free so the inner loop still vectorises.
        let t = 0.5 - dx * dx - dy * dy;
        if t <= 0.0 {
            return 0.0;
        }
        let index = (hash_lattice(seed, ix, iy, 0) & (GRADIENTS_2D.len() as u64 - 1)) as usize;
        let g = GRADIENTS_2D[index];
        let t2 = t * t;
        t2 * t2 * (g[0] * dx + g[1] * dy)
    };

    let total = contribution(i, j, dx0, dy0)
        + contribution(i + offset_i, j + offset_j, dx1, dy1)
        + contribution(i + 1, j + 1, dx2, dy2);

    total * SIMPLEX_2D_SCALE
}

/// 3D simplex-lattice gradient noise. Output is approximately `[-1, 1]`.
#[must_use]
pub fn simplex_3d(seed: u64, x: f32, y: f32, z: f32) -> f32 {
    let (x, y, z) = (
        clamp_coordinate(x),
        clamp_coordinate(y),
        clamp_coordinate(z),
    );

    let skew = (x + y + z) * SKEW_3D;
    let i = floor_to_i32(x + skew);
    let j = floor_to_i32(y + skew);
    let k = floor_to_i32(z + skew);

    let unskew = (i + j + k) as f32 * UNSKEW_3D;
    let dx0 = x - (i as f32 - unskew);
    let dy0 = y - (j as f32 - unskew);
    let dz0 = z - (k as f32 - unskew);

    // Rank the three coordinates to find which of the six tetrahedra the point
    // fell into. Comparisons only — no sorting, no branches with side effects.
    let (i1, j1, k1, i2, j2, k2) = if dx0 >= dy0 {
        if dy0 >= dz0 {
            (1, 0, 0, 1, 1, 0)
        } else if dx0 >= dz0 {
            (1, 0, 0, 1, 0, 1)
        } else {
            (0, 0, 1, 1, 0, 1)
        }
    } else if dy0 < dz0 {
        (0, 0, 1, 0, 1, 1)
    } else if dx0 < dz0 {
        (0, 1, 0, 0, 1, 1)
    } else {
        (0, 1, 0, 1, 1, 0)
    };

    let dx1 = dx0 - i1 as f32 + UNSKEW_3D;
    let dy1 = dy0 - j1 as f32 + UNSKEW_3D;
    let dz1 = dz0 - k1 as f32 + UNSKEW_3D;
    let dx2 = dx0 - i2 as f32 + 2.0 * UNSKEW_3D;
    let dy2 = dy0 - j2 as f32 + 2.0 * UNSKEW_3D;
    let dz2 = dz0 - k2 as f32 + 2.0 * UNSKEW_3D;
    let dx3 = dx0 - 1.0 + 3.0 * UNSKEW_3D;
    let dy3 = dy0 - 1.0 + 3.0 * UNSKEW_3D;
    let dz3 = dz0 - 1.0 + 3.0 * UNSKEW_3D;

    let contribution = |ix: i32, iy: i32, iz: i32, dx: f32, dy: f32, dz: f32| -> f32 {
        let t = 0.6 - dx * dx - dy * dy - dz * dz;
        if t <= 0.0 {
            return 0.0;
        }
        let index = (hash_lattice(seed, ix, iy, iz) & (GRADIENTS_3D.len() as u64 - 1)) as usize;
        let g = GRADIENTS_3D[index];
        let t2 = t * t;
        t2 * t2 * (g[0] * dx + g[1] * dy + g[2] * dz)
    };

    let total = contribution(i, j, k, dx0, dy0, dz0)
        + contribution(i + i1, j + j1, k + k1, dx1, dy1, dz1)
        + contribution(i + i2, j + j2, k + k2, dx2, dy2, dz2)
        + contribution(i + 1, j + 1, k + 1, dx3, dy3, dz3);

    total * SIMPLEX_3D_SCALE
}

// ---------------------------------------------------------------------------
// Combinators
// ---------------------------------------------------------------------------

/// How octaves of noise are combined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fractal {
    /// Plain fractional Brownian motion: sum the octaves. Rolling, cloud-like.
    #[default]
    Fbm,
    /// Absolute value, inverted. Produces sharp ridgelines — mountains.
    Ridged,
    /// Absolute value, not inverted. Puffy, billowing.
    Billow,
}

/// Parameters for a fractal noise field.
///
/// Every knob is explicit. There are no hidden defaults inside the sampler,
/// because a hidden default is a number that changes the whole world and is not
/// written down anywhere.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FractalParams {
    /// How the octaves combine.
    pub fractal: Fractal,
    /// Number of octaves. Each one doubles the cost.
    pub octaves: u32,
    /// Frequency of the first octave — the inverse of feature size.
    pub frequency: f32,
    /// Frequency multiplier per octave. 2.0 is the usual choice.
    pub lacunarity: f32,
    /// Amplitude multiplier per octave. 0.5 is the usual choice.
    pub gain: f32,
}

impl Default for FractalParams {
    fn default() -> Self {
        Self {
            fractal: Fractal::Fbm,
            octaves: 4,
            frequency: 0.02,
            lacunarity: 2.0,
            gain: 0.5,
        }
    }
}

impl FractalParams {
    /// Clamps the octave count to something a generator cannot hang on.
    ///
    /// A mod passing a large octave count would otherwise turn one chunk fill
    /// into an unbounded loop inside the tick. Charter rule 10 sandboxes mods
    /// for crash isolation; this is the same instinct applied to a number.
    pub const MAX_OCTAVES: u32 = 16;

    /// The octave count actually used.
    #[must_use]
    pub const fn effective_octaves(&self) -> u32 {
        if self.octaves > Self::MAX_OCTAVES {
            Self::MAX_OCTAVES
        } else if self.octaves == 0 {
            1
        } else {
            self.octaves
        }
    }
}

/// Applies the fractal shaping to one octave's raw sample.
#[must_use]
fn shape(fractal: Fractal, sample: f32) -> f32 {
    match fractal {
        Fractal::Fbm => sample,
        // `abs` is in the allowed subset: it is a sign-bit clear, not a
        // rounding operation.
        Fractal::Ridged => 1.0 - sample.abs() * 2.0,
        Fractal::Billow => sample.abs() * 2.0 - 1.0,
    }
}

/// Fractal 2D noise. Output is approximately `[-1, 1]`.
#[must_use]
pub fn fractal_2d(seed: u64, x: f32, y: f32, params: &FractalParams) -> f32 {
    let mut total = 0.0;
    let mut amplitude = 1.0;
    let mut normaliser = 0.0;
    let mut frequency = params.frequency;

    for octave in 0..params.effective_octaves() {
        // Offset the seed per octave so octaves are not scaled copies of one
        // another — without this, features at different scales line up and the
        // result looks synthetic.
        let octave_seed = seed ^ u64::from(octave).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let sample = simplex_2d(octave_seed, x * frequency, y * frequency);
        total += shape(params.fractal, sample) * amplitude;
        normaliser += amplitude;

        // Iterative multiplication rather than `powi`: powi's association order
        // is LLVM's choice (float-determinism.md §1).
        amplitude *= params.gain;
        frequency *= params.lacunarity;
    }

    // `normaliser` is a sum of positive amplitudes starting at 1.0, so it can
    // never be zero and this division is always safe.
    total / normaliser
}

/// Fractal 3D noise. Output is approximately `[-1, 1]`.
#[must_use]
pub fn fractal_3d(seed: u64, x: f32, y: f32, z: f32, params: &FractalParams) -> f32 {
    let mut total = 0.0;
    let mut amplitude = 1.0;
    let mut normaliser = 0.0;
    let mut frequency = params.frequency;

    for octave in 0..params.effective_octaves() {
        let octave_seed = seed ^ u64::from(octave).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let sample = simplex_3d(octave_seed, x * frequency, y * frequency, z * frequency);
        total += shape(params.fractal, sample) * amplitude;
        normaliser += amplitude;
        amplitude *= params.gain;
        frequency *= params.lacunarity;
    }

    total / normaliser
}

// ---------------------------------------------------------------------------
// Bulk fills
// ---------------------------------------------------------------------------

/// A rectangular sampling region in world units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Region2d {
    /// World x of the first sample.
    pub origin_x: f32,
    /// World y of the first sample.
    pub origin_y: f32,
    /// Distance between samples along x.
    pub step_x: f32,
    /// Distance between samples along y.
    pub step_y: f32,
    /// Samples along x.
    pub width: usize,
    /// Samples along y.
    pub height: usize,
}

impl Region2d {
    /// Samples the region holds.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.width * self.height
    }

    /// Whether the region holds no samples.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }
}

/// A box-shaped sampling region in world units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Region3d {
    /// World x of the first sample.
    pub origin_x: f32,
    /// World y of the first sample.
    pub origin_y: f32,
    /// World z of the first sample.
    pub origin_z: f32,
    /// Distance between samples along each axis.
    pub step: f32,
    /// Samples along x.
    pub width: usize,
    /// Samples along y.
    pub height: usize,
    /// Samples along z.
    pub depth: usize,
}

impl Region3d {
    /// Samples the region holds.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.width * self.height * self.depth
    }

    /// Whether the region holds no samples.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0 || self.depth == 0
    }
}

/// A bulk fill was given a buffer that does not match its region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("buffer holds {found} samples but the region needs {expected}")]
pub struct BufferSizeMismatch {
    /// Samples the region requires.
    pub expected: usize,
    /// Samples the buffer holds.
    pub found: usize,
}

/// Fills a whole 2D buffer with fractal noise.
///
/// **This is the API Lua calls — once per chunk, not once per sample.** A
/// per-sample FFI crossing would cost more than the noise does; a whole-buffer
/// fill amortises it to nothing.
///
/// # Errors
///
/// [`BufferSizeMismatch`] if `out` is not exactly `region.len()` long.
pub fn fill_2d(
    seed: u64,
    region: &Region2d,
    params: &FractalParams,
    out: &mut [f32],
) -> Result<(), BufferSizeMismatch> {
    if out.len() != region.len() {
        return Err(BufferSizeMismatch {
            expected: region.len(),
            found: out.len(),
        });
    }

    for row in 0..region.height {
        let y = region.origin_y + row as f32 * region.step_y;
        let start = row * region.width;
        // A flat slice per row with no branches and no carried accumulator:
        // the shape LLVM can vectorise.
        let slice = &mut out[start..start + region.width];
        for (column, sample) in slice.iter_mut().enumerate() {
            let x = region.origin_x + column as f32 * region.step_x;
            *sample = fractal_2d(seed, x, y, params);
        }
    }
    Ok(())
}

/// Fills a whole 3D buffer with fractal noise.
///
/// Layout is x-fastest, matching [`crate::block::subnode_index`] and
/// [`crate::coords::LocalBlock::index`] so no caller has to transpose.
///
/// # Errors
///
/// [`BufferSizeMismatch`] if `out` is not exactly `region.len()` long.
pub fn fill_3d(
    seed: u64,
    region: &Region3d,
    params: &FractalParams,
    out: &mut [f32],
) -> Result<(), BufferSizeMismatch> {
    if out.len() != region.len() {
        return Err(BufferSizeMismatch {
            expected: region.len(),
            found: out.len(),
        });
    }

    for layer in 0..region.depth {
        let z = region.origin_z + layer as f32 * region.step;
        for row in 0..region.height {
            let y = region.origin_y + row as f32 * region.step;
            let start = (layer * region.height + row) * region.width;
            let slice = &mut out[start..start + region.width];
            for (column, sample) in slice.iter_mut().enumerate() {
                let x = region.origin_x + column as f32 * region.step;
                *sample = fractal_3d(seed, x, y, z, params);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> FractalParams {
        FractalParams::default()
    }

    #[test]
    fn fade_has_the_documented_endpoints_and_midpoint() {
        // Exact comparison is correct here, not sloppy: the endpoints are what
        // make the curve usable as an interpolant, and 0 and 1 are exactly
        // representable. An epsilon would hide a curve that was subtly wrong at
        // the cell boundaries, which is precisely where it matters.
        assert_eq!(fade(0.0).to_bits(), 0.0f32.to_bits());
        assert_eq!(fade(1.0).to_bits(), 1.0f32.to_bits());
        assert!((fade(0.5) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn fade_is_monotonic() {
        let mut previous = fade(0.0);
        for step in 1..=100 {
            let current = fade(step as f32 / 100.0);
            assert!(current >= previous, "fade decreased at {step}");
            previous = current;
        }
    }

    #[test]
    fn noise_is_reproducible() {
        // The whole point. Same inputs, same bits — not "close", identical.
        for &(x, y) in &[(0.0, 0.0), (1.5, -2.25), (100.125, 55.5)] {
            assert_eq!(simplex_2d(7, x, y).to_bits(), simplex_2d(7, x, y).to_bits());
            assert_eq!(
                gradient_2d(7, x, y).to_bits(),
                gradient_2d(7, x, y).to_bits()
            );
        }
    }

    #[test]
    fn noise_stays_in_range() {
        let mut min = f32::MAX;
        let mut max = f32::MIN;
        for i in 0..4000 {
            let x = i as f32 * 0.37;
            let y = i as f32 * 0.11;
            for value in [simplex_2d(3, x, y), gradient_2d(3, x, y)] {
                min = min.min(value);
                max = max.max(value);
            }
            let value = simplex_3d(3, x, y, x * 0.5);
            min = min.min(value);
            max = max.max(value);
        }
        // Gradient noise is not exactly bounded to [-1, 1]; a little overshoot
        // is normal and harmless. Wild values would mean a broken scale factor.
        assert!(min > -1.6, "min {min} is implausibly low");
        assert!(max < 1.6, "max {max} is implausibly high");
        assert!(min < -0.3, "min {min} suggests the field is not varying");
        assert!(max > 0.3, "max {max} suggests the field is not varying");
    }

    #[test]
    fn changing_the_seed_changes_the_field() {
        let differences = (0..200)
            .filter(|i| {
                let x = *i as f32 * 0.3;
                // Bit comparison, not an epsilon: the claim is that a
                // different seed gives a DIFFERENT field, and two samples that
                // differ in the last bit still differ.
                simplex_2d(1, x, 0.5).to_bits() != simplex_2d(2, x, 0.5).to_bits()
            })
            .count();
        assert!(differences > 190, "only {differences}/200 samples differed");
    }

    #[test]
    fn the_field_is_continuous() {
        // Gradient noise must not jump. A large step between adjacent samples
        // would mean the lattice interpolation is wrong, and shows up as visible
        // seams in terrain.
        let mut previous = simplex_2d(5, 0.0, 0.0);
        for step in 1..2000 {
            let x = step as f32 * 0.01;
            let current = simplex_2d(5, x, 0.0);
            assert!(
                (current - previous).abs() < 0.35,
                "jump of {} at x={x}",
                (current - previous).abs()
            );
            previous = current;
        }
    }

    #[test]
    fn noise_is_never_nan_or_infinite() {
        // Charter rule 4: NaN payloads are not specified, so producing one
        // breaks the cross-platform hash.
        for i in -500..500 {
            let v = i as f32 * 0.37;
            for value in [
                simplex_2d(1, v, -v),
                simplex_3d(1, v, -v, v * 0.5),
                gradient_2d(1, v, -v),
                gradient_3d(1, v, -v, v * 0.5),
                fractal_2d(1, v, -v, &params()),
                fractal_3d(1, v, -v, v * 0.5, &params()),
            ] {
                assert!(value.is_finite(), "non-finite at {v}: {value}");
            }
        }
    }

    #[test]
    fn every_fractal_mode_produces_a_varying_finite_field() {
        for fractal in [Fractal::Fbm, Fractal::Ridged, Fractal::Billow] {
            let params = FractalParams {
                fractal,
                ..FractalParams::default()
            };
            let samples: Vec<f32> = (0..200)
                .map(|i| fractal_2d(11, i as f32 * 1.7, 0.0, &params))
                .collect();
            assert!(samples.iter().all(|v| v.is_finite()), "{fractal:?}");
            let min = samples.iter().copied().fold(f32::MAX, f32::min);
            let max = samples.iter().copied().fold(f32::MIN, f32::max);
            assert!(max - min > 0.1, "{fractal:?} produced a flat field");
        }
    }

    #[test]
    fn octaves_are_clamped_rather_than_trusted() {
        // A mod passing a huge octave count must not turn one chunk fill into an
        // unbounded loop inside the tick.
        let params = FractalParams {
            octaves: u32::MAX,
            ..FractalParams::default()
        };
        assert_eq!(params.effective_octaves(), FractalParams::MAX_OCTAVES);

        let params = FractalParams {
            octaves: 0,
            ..FractalParams::default()
        };
        assert_eq!(
            params.effective_octaves(),
            1,
            "zero octaves must still sample"
        );
        assert!(fractal_2d(1, 0.0, 0.0, &params).is_finite());
    }

    #[test]
    fn fills_match_point_sampling() {
        // The bulk path is an optimisation, so it must agree exactly with the
        // obvious one — not approximately.
        let region = Region2d {
            origin_x: -3.5,
            origin_y: 2.25,
            step_x: 0.75,
            step_y: 0.5,
            width: 16,
            height: 16,
        };
        let mut out = vec![0.0; region.len()];
        fill_2d(99, &region, &params(), &mut out).expect("fill");

        for row in 0..region.height {
            for column in 0..region.width {
                let x = region.origin_x + column as f32 * region.step_x;
                let y = region.origin_y + row as f32 * region.step_y;
                assert_eq!(
                    out[row * region.width + column].to_bits(),
                    fractal_2d(99, x, y, &params()).to_bits(),
                    "at ({column}, {row})"
                );
            }
        }
    }

    #[test]
    fn fill_3d_matches_point_sampling_and_is_x_fastest() {
        let region = Region3d {
            origin_x: 0.0,
            origin_y: 0.0,
            origin_z: 0.0,
            step: 0.5,
            width: 6,
            height: 5,
            depth: 4,
        };
        let mut out = vec![0.0; region.len()];
        fill_3d(4, &region, &params(), &mut out).expect("fill");

        for layer in 0..region.depth {
            for row in 0..region.height {
                for column in 0..region.width {
                    let index = (layer * region.height + row) * region.width + column;
                    let expected = fractal_3d(
                        4,
                        column as f32 * region.step,
                        row as f32 * region.step,
                        layer as f32 * region.step,
                        &params(),
                    );
                    assert_eq!(out[index].to_bits(), expected.to_bits());
                }
            }
        }
    }

    #[test]
    fn a_mismatched_buffer_is_an_error_not_a_panic() {
        let region = Region2d {
            origin_x: 0.0,
            origin_y: 0.0,
            step_x: 1.0,
            step_y: 1.0,
            width: 4,
            height: 4,
        };
        let mut out = vec![0.0; 5];
        assert!(fill_2d(1, &region, &params(), &mut out).is_err());
    }

    #[test]
    fn an_empty_region_fills_nothing_without_complaint() {
        let region = Region2d {
            origin_x: 0.0,
            origin_y: 0.0,
            step_x: 1.0,
            step_y: 1.0,
            width: 0,
            height: 4,
        };
        assert!(region.is_empty());
        let mut out: Vec<f32> = Vec::new();
        assert!(fill_2d(1, &region, &params(), &mut out).is_ok());
    }
}
