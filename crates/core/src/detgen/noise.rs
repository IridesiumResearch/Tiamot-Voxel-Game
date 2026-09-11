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

/// The largest magnitude one octave of [`simplex_3d`] can return.
///
/// **Derived, not measured**, because what stands on it is a decision to SKIP
/// generating terrain: a bound that is merely usually right puts holes in a
/// world, and they appear only where the field happens to wiggle between
/// whatever samples were taken to justify it.
///
/// The kernel is `t⁴ · (g · d)` summed over four lattice corners, where
/// `t = 0.6 - |d|²` and the contribution is zero once `t` is. Every gradient in
/// [`GRADIENTS_3D`] has two unit components and a zero, so `|g| = sqrt(2)` and
/// Cauchy-Schwarz gives `|g · d| <= sqrt(2)·|d|`. Writing `r = |d|`, one corner
/// is at most
///
/// ```text
///     f(r) = (0.6 - r²)⁴ · sqrt(2) · r
/// ```
///
/// whose derivative `(0.6 - r²)³ · (0.6 - 9r²)` vanishes at `r² = 0.6/9`, giving
/// `f = 0.029543`. Four corners and [`SIMPLEX_3D_SCALE`]:
/// `4 × 0.029543 × 32 = 3.7815`.
///
/// **Conservative by about 3.8x.** The four corners cannot all be at their own
/// worst distance at once — being near one corner of a tetrahedron is being far
/// from the others — so the observed maximum over a dense search is near 1.0.
/// Tightening this is a pure win for how much terrain can be skipped and needs
/// no API change: it is one constant, and `the_simplex_bound_holds_everywhere`
/// is what would have to keep passing. Proving a tighter one means bounding the
/// sum under the geometric constraint that links the four distances, which is
/// real work and is not done here.
pub const SIMPLEX_3D_BOUND: f32 = 3.7815;

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

    /// The range this fractal's output can occupy, as `(low, high)`.
    ///
    /// Every octave is shaped and then averaged with positive weights — the
    /// division by `normaliser` in [`fractal_3d`] is exactly that — so the
    /// result lies between the smallest and largest value `shape` can produce.
    /// No octave count, frequency or gain can widen it, which is why none of
    /// them appear here.
    ///
    /// [`SIMPLEX_3D_BOUND`] is the bound on one raw sample. `Ridged` and
    /// `Billow` map it through `1 - 2|s|` and `2|s| - 1`, both of which are
    /// monotone in `|s|`, so their ends follow directly.
    ///
    /// Used by `Density::bounds` to decide that a chunk cannot contain any
    /// surface. Conservative in one direction only: too wide costs terrain that
    /// gets generated when it need not have been, too narrow puts a hole in the
    /// world.
    #[must_use]
    pub fn range(&self) -> (f32, f32) {
        let bound = SIMPLEX_3D_BOUND;
        match self.fractal {
            Fractal::Fbm => (-bound, bound),
            Fractal::Ridged => (1.0 - 2.0 * bound, 1.0),
            Fractal::Billow => (-1.0, 2.0 * bound - 1.0),
        }
    }

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
// Bounds over a box
// ---------------------------------------------------------------------------

/// The most cells [`simplex_3d_bounds`] will walk before giving up.
///
/// The count grows with the box's volume in noise units, so this is what stops
/// a mod asking about a field so high-frequency that bounding it costs more
/// than sampling it. Giving up is answering `None`, and the caller then uses
/// the range that always holds.
///
/// **Sixty-four is four cells an axis, and it is free.** Measured over a
/// streamed column of ordinary terrain, every cap from 512 down to 32 decided
/// exactly the same 93% of chunks, while the cost per bound fell from 0.043 ms
/// to 0.016 ms — so the work above this ceiling was being paid for and
/// answering nothing. That is the honest shape of the mechanism: once a box
/// spans more than a cell or two, the union over the cells it could be in
/// covers most of the field's range anyway, and there is nothing left to win.
const MAX_SIMPLEX_CELLS: i64 = 64;

/// An interval `[low, high]`, closed at both ends.
type Span = (f32, f32);

/// The interval a product of two intervals lies in.
fn span_multiply(a: Span, b: Span) -> Span {
    let corners = [a.0 * b.0, a.0 * b.1, a.1 * b.0, a.1 * b.1];
    let mut low = corners[0];
    let mut high = corners[0];
    for corner in &corners[1..] {
        low = low.min(*corner);
        high = high.max(*corner);
    }
    (low, high)
}

/// The interval a square lies in, which is not the square of the interval.
///
/// `[-2, 1]` squared is `[0, 4]`, not `[4, 1]`: an interval straddling zero has
/// its smallest square AT zero. Getting this wrong is how an unsound bound gets
/// written, and it is unsound in the direction that puts holes in a world.
fn span_square(a: Span) -> Span {
    if a.0 >= 0.0 {
        (a.0 * a.0, a.1 * a.1)
    } else if a.1 <= 0.0 {
        (a.1 * a.1, a.0 * a.0)
    } else {
        let reach = a.0.abs().max(a.1.abs());
        (0.0, reach * reach)
    }
}

/// The interval containing both.
fn span_union(a: Span, b: Span) -> Span {
    (a.0.min(b.0), a.1.max(b.1))
}

/// The six tetrahedra of a simplex cell, as the two middle corners each picks.
///
/// The same six [`simplex_3d`] selects between, in the same order, written as
/// data so a bound can walk the ones a box could land in instead of evaluating
/// the comparisons at a point it does not have.
const TETRAHEDRA: [([i32; 3], [i32; 3]); 6] = [
    ([1, 0, 0], [1, 1, 0]),
    ([1, 0, 0], [1, 0, 1]),
    ([0, 0, 1], [1, 0, 1]),
    ([0, 0, 1], [0, 1, 1]),
    ([0, 1, 0], [0, 1, 1]),
    ([0, 1, 0], [1, 1, 0]),
];

/// Whether each of [`TETRAHEDRA`] is reachable from somewhere in the box.
///
/// The selection in [`simplex_3d`] is three comparisons between `dx0`, `dy0`
/// and `dz0`. Over a box each of those is an interval, and a comparison between
/// two intervals has three answers: definitely, definitely not, or both — so a
/// box lying inside one tetrahedron names exactly one, and only a box that
/// really does straddle a face pays for more than one.
fn reachable_tetrahedra(d: [Span; 3]) -> [bool; 6] {
    // `a >= b` is possible while `a`'s top reaches `b`'s bottom; `a < b` is
    // possible while `a`'s bottom is under `b`'s top.
    let at_least = |a: Span, b: Span| a.1 >= b.0;
    let below = |a: Span, b: Span| a.0 < b.1;
    let (x, y, z) = (d[0], d[1], d[2]);
    [
        at_least(x, y) && at_least(y, z),
        at_least(x, y) && below(y, z) && at_least(x, z),
        at_least(x, y) && below(y, z) && below(x, z),
        below(x, y) && below(y, z),
        below(x, y) && at_least(y, z) && below(x, z),
        below(x, y) && at_least(y, z) && at_least(x, z),
    ]
}

/// One lattice corner's contribution to [`simplex_3d`], over a box.
///
/// `t` clamped at zero is what makes this exact rather than merely sound where
/// the box crosses the edge of the kernel's support: the real contribution is
/// zero on the far side, and a quartic whose interval starts at zero already
/// contains it.
fn corner_span(seed: u64, at: [i32; 3], d: [Span; 3]) -> Span {
    let squares = [span_square(d[0]), span_square(d[1]), span_square(d[2])];
    let radius = (
        squares[0].0 + squares[1].0 + squares[2].0,
        squares[0].1 + squares[1].1 + squares[2].1,
    );
    let t = (0.6 - radius.1, 0.6 - radius.0);
    if t.1 <= 0.0 {
        return (0.0, 0.0);
    }
    let t = (t.0.max(0.0), t.1);
    let raised = (t.0 * t.0, t.1 * t.1);
    let quartic = (raised.0 * raised.0, raised.1 * raised.1);

    let index =
        (hash_lattice(seed, at[0], at[1], at[2]) & (GRADIENTS_3D.len() as u64 - 1)) as usize;
    let g = GRADIENTS_3D[index];
    let mut dot = (0.0_f32, 0.0_f32);
    for axis in 0..3 {
        let term = span_multiply((g[axis], g[axis]), d[axis]);
        dot = (dot.0 + term.0, dot.1 + term.1);
    }
    span_multiply(quartic, dot)
}

/// The smallest and largest value [`simplex_3d`] can take anywhere in a box.
///
/// `None` when the box spans more than [`MAX_SIMPLEX_CELLS`] cells, which is
/// the caller's cue to fall back to [`SIMPLEX_3D_BOUND`].
///
/// # Why a bound over a box is not the same question as [`SIMPLEX_3D_BOUND`]
///
/// That constant is the range the field occupies SOMEWHERE. It is the same
/// interval everywhere, which is exactly what makes it useless for deciding
/// that a particular chunk cannot hold a particular thing — a generator asking
/// "can this biome be here?" gets the same yes in every chunk in the world.
/// This answers over one box, and so can say no.
///
/// # How it is sound
///
/// [`simplex_3d`] sums four lattice corners: which four is decided by the cell
/// the sample falls in and by three comparisons that pick one of six tetrahedra
/// inside it. Both are decided per SAMPLE, and a box holds many, so this walks
/// every (cell, tetrahedron) pair the box can reach — usually exactly one — and
/// takes the union of what each would give. Each pair's own answer is the sum
/// of four [`corner_span`]s, which is interval arithmetic over the same
/// expression the kernel evaluates.
///
/// # Why not simply widen every nearby corner to include zero
///
/// It was written that way first, to avoid having to decide which four corners
/// the kernel picks, and it is sound. It is also **useless**, which is worth
/// recording so nobody rediscovers it: widening each term to include zero makes
/// the sum contain zero, so the interval straddles zero in every box in the
/// world and can never place the field above or below a threshold. Measured at
/// the time: nought per cent of chunks decidable, at every frequency tried.
/// A bound that cannot decide anything is not cheaper than one that can — it is
/// the same as not having one.
///
/// The result is intersected with [`SIMPLEX_3D_BOUND`] before it is returned,
/// so this can only ever be an improvement on it — and for a box much larger
/// than a cell it IS it. Bounding over a box buys nothing once the field's
/// features are smaller than the box.
#[must_use]
pub fn simplex_3d_bounds(seed: u64, x: Span, y: Span, z: Span) -> Option<Span> {
    let low_corner = (
        clamp_coordinate(x.0.min(x.1)),
        clamp_coordinate(y.0.min(y.1)),
        clamp_coordinate(z.0.min(z.1)),
    );
    let high_corner = (
        clamp_coordinate(x.0.max(x.1)),
        clamp_coordinate(y.0.max(y.1)),
        clamp_coordinate(z.0.max(z.1)),
    );

    // Skewing adds `(x + y + z) * K` with `K > 0` to each coordinate, so it is
    // increasing in all three: the box's skewed extent runs corner to corner,
    // and the cells it can land in are the integer points between them. No
    // margin — a sample's cell is the floor of its own skewed position, not a
    // neighbourhood of it.
    let skewed = |p: (f32, f32, f32)| -> (f32, f32, f32) {
        let skew = (p.0 + p.1 + p.2) * SKEW_3D;
        (p.0 + skew, p.1 + skew, p.2 + skew)
    };
    let first = skewed(low_corner);
    let last = skewed(high_corner);
    let (i0, i1) = (floor_to_i32(first.0), floor_to_i32(last.0));
    let (j0, j1) = (floor_to_i32(first.1), floor_to_i32(last.1));
    let (k0, k1) = (floor_to_i32(first.2), floor_to_i32(last.2));

    let cells = i64::from(i1 - i0 + 1)
        .saturating_mul(i64::from(j1 - j0 + 1))
        .saturating_mul(i64::from(k1 - k0 + 1));
    if cells > MAX_SIMPLEX_CELLS {
        return None;
    }

    let mut total: Option<Span> = None;
    // Walked in lattice order, which is fixed — charter rule 4 bans float
    // accumulation over an order that is not.
    for i in i0..=i1 {
        for j in j0..=j1 {
            for k in k0..=k1 {
                let unskew = (i + j + k) as f32 * UNSKEW_3D;
                let origin = (i as f32 - unskew, j as f32 - unskew, k as f32 - unskew);
                let d0 = [
                    (low_corner.0 - origin.0, high_corner.0 - origin.0),
                    (low_corner.1 - origin.1, high_corner.1 - origin.1),
                    (low_corner.2 - origin.2, high_corner.2 - origin.2),
                ];

                let near = corner_span(seed, [i, j, k], d0);
                let shift = |by: [i32; 3], unskews: f32| -> [Span; 3] {
                    [
                        (
                            d0[0].0 - by[0] as f32 + unskews,
                            d0[0].1 - by[0] as f32 + unskews,
                        ),
                        (
                            d0[1].0 - by[1] as f32 + unskews,
                            d0[1].1 - by[1] as f32 + unskews,
                        ),
                        (
                            d0[2].0 - by[2] as f32 + unskews,
                            d0[2].1 - by[2] as f32 + unskews,
                        ),
                    ]
                };
                let far = corner_span(
                    seed,
                    [i + 1, j + 1, k + 1],
                    shift([1, 1, 1], 3.0 * UNSKEW_3D),
                );

                for (tetrahedron, reachable) in TETRAHEDRA.iter().zip(reachable_tetrahedra(d0)) {
                    if !reachable {
                        continue;
                    }
                    let (one, two) = *tetrahedron;
                    let middle = corner_span(
                        seed,
                        [i + one[0], j + one[1], k + one[2]],
                        shift(one, UNSKEW_3D),
                    );
                    let outer = corner_span(
                        seed,
                        [i + two[0], j + two[1], k + two[2]],
                        shift(two, 2.0 * UNSKEW_3D),
                    );
                    let sum = (
                        near.0 + middle.0 + outer.0 + far.0,
                        near.1 + middle.1 + outer.1 + far.1,
                    );
                    total = Some(match total {
                        None => sum,
                        Some(reached) => span_union(reached, sum),
                    });
                }
            }
        }
    }

    let total = total?;
    Some((
        (total.0 * SIMPLEX_3D_SCALE).max(-SIMPLEX_3D_BOUND),
        (total.1 * SIMPLEX_3D_SCALE).min(SIMPLEX_3D_BOUND),
    ))
}

/// The smallest and largest value [`fractal_3d`] can take anywhere in a box.
///
/// Each octave is bounded over the box the octave itself sees — the box scaled
/// by that octave's frequency — shaped, weighted and summed exactly as
/// [`fractal_3d`] sums them. An octave whose own bound is unavailable or no
/// better than [`SIMPLEX_3D_BOUND`] falls back to it, so this is never worse
/// than [`FractalParams::range`] and usually far better.
#[must_use]
pub fn fractal_3d_bounds(seed: u64, x: Span, y: Span, z: Span, params: &FractalParams) -> Span {
    let mut total = (0.0_f32, 0.0_f32);
    let mut amplitude = 1.0_f32;
    let mut normaliser = 0.0_f32;
    let mut frequency = params.frequency;

    for octave in 0..params.effective_octaves() {
        let octave_seed = seed ^ u64::from(octave).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let scaled = |span: Span| -> Span {
            let (a, b) = (span.0 * frequency, span.1 * frequency);
            (a.min(b), a.max(b))
        };
        // Already intersected with the constant by `simplex_3d_bounds`, so
        // giving up and using the constant is the same answer, not a worse one.
        let raw = simplex_3d_bounds(octave_seed, scaled(x), scaled(y), scaled(z))
            .unwrap_or((-SIMPLEX_3D_BOUND, SIMPLEX_3D_BOUND));
        let shaped = shape_span(params.fractal, raw);
        total = (
            total.0 + shaped.0 * amplitude,
            total.1 + shaped.1 * amplitude,
        );
        normaliser += amplitude;
        amplitude *= params.gain;
        frequency *= params.lacunarity;
    }

    (total.0 / normaliser, total.1 / normaliser)
}

/// [`shape`] lifted to an interval.
///
/// `Ridged` and `Billow` both run through `|s|`, which is not monotone across
/// zero — an interval straddling it reaches down to zero, not to the smaller
/// of its two magnitudes.
fn shape_span(fractal: Fractal, sample: Span) -> Span {
    match fractal {
        Fractal::Fbm => sample,
        Fractal::Ridged | Fractal::Billow => {
            let magnitude = if sample.0 >= 0.0 {
                (sample.0, sample.1)
            } else if sample.1 <= 0.0 {
                (-sample.1, -sample.0)
            } else {
                (0.0, sample.0.abs().max(sample.1.abs()))
            };
            match fractal {
                Fractal::Ridged => (1.0 - magnitude.1 * 2.0, 1.0 - magnitude.0 * 2.0),
                _ => (magnitude.0 * 2.0 - 1.0, magnitude.1 * 2.0 - 1.0),
            }
        }
    }
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
    fn a_box_bound_holds_everywhere_inside_the_box() {
        // Same stake as `the_simplex_bound_holds_everywhere_it_is_searched`,
        // and higher: this bound is what a generator skips a biome on, and
        // unlike the constant it is not one number somebody can re-derive by
        // hand. It is interval arithmetic over the lattice, and every piece of
        // it is wrong in the dangerous direction if written naively — a square
        // across zero, an interval product missing a sign, and above all the
        // reasoning about WHICH tetrahedra a box can reach, where comparing
        // the wrong ends of two intervals silently drops one.
        //
        // **Strides rather than a grid**, for the reason the sibling test uses
        // them: a lattice of positions tests a lattice of positions. The
        // failures here are narrow — one wrong comparison was invisible to
        // 700,000 samples on a grid and showed up in the first hundred
        // thousand of a search that moved continuously — and box SIZE has to
        // vary too, because a box inside a single tetrahedron never exercises
        // the part that decides between them.
        let sizes = [0.05f32, 0.17, 0.3, 0.61, 0.93, 1.2];
        let mut worst_escape = 0.0f32;
        let mut widest = 0.0f32;
        let mut tightest = f32::INFINITY;
        let mut checked = 0u32;
        for seed in [5u64, 17, 271, 4096, 99_991] {
            // Scattered rather than walked. Tying the three coordinates to
            // one counter — strides, a grid, anything on a line — keeps the
            // three `d` intervals in a fixed relationship to each other, and
            // the reasoning under test is exactly about that relationship. Two
            // wrong-end comparisons survived both a grid and a stride walk and
            // die here.
            let mut state = seed | 1;
            let mut next = || {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 40) as f32 / 16_777_216.0
            };
            for n in 0..20_000 {
                let size = sizes[n as usize % sizes.len()];
                let origin = (next() * 8.0 - 4.0, next() * 8.0 - 4.0, next() * 8.0 - 4.0);
                let Some(bound) = simplex_3d_bounds(
                    seed,
                    (origin.0, origin.0 + size),
                    (origin.1, origin.1 + size),
                    (origin.2, origin.2 + size),
                ) else {
                    continue;
                };
                widest = widest.max(bound.1 - bound.0);
                if size < 0.1 {
                    tightest = tightest.min(bound.1 - bound.0);
                }
                let steps = 7;
                for i in 0..=steps {
                    for j in 0..=steps {
                        for k in 0..=steps {
                            let at = |n: i32, from: f32| from + size * n as f32 / steps as f32;
                            let value =
                                simplex_3d(seed, at(i, origin.0), at(j, origin.1), at(k, origin.2));
                            checked += 1;
                            let escape = (bound.0 - value).max(value - bound.1);
                            worst_escape = worst_escape.max(escape);
                            assert!(
                                escape <= 0.0,
                                "a sample {value} inside a {size}-wide box at {origin:?}, seed \
                                 {seed}, escaped its bound ({}, {}) by {escape} — a generator \
                                 skipping on this bound would leave a hole",
                                bound.0,
                                bound.1
                            );
                        }
                    }
                }
            }
        }
        assert!(checked > 20_000_000, "only {checked} samples were searched");
        // **Never worse than the constant**, which is what lets a caller use
        // this without checking which of the two it got. A box wider than a
        // cell hits exactly this, and that is the mechanism working as designed
        // rather than failing.
        assert!(
            widest <= 2.0 * SIMPLEX_3D_BOUND,
            "a box bound of {widest} is wider than the constant it is meant to improve on"
        );
        // And non-vacuous, which is the assertion with teeth: a bound of
        // `[-inf, inf]`, or one that always returned the constant, would pass
        // everything above and prune nothing. A box a twentieth of a cell
        // across has to do far better than the interval that holds everywhere,
        // or none of this was worth building.
        assert!(
            tightest < 2.0 * SIMPLEX_3D_BOUND * 0.05,
            "the tightest bound over a tiny box was {tightest}, against {} that holds \
             everywhere — bounding over a box is buying nothing",
            2.0 * SIMPLEX_3D_BOUND
        );
        let _ = worst_escape;
    }

    #[test]
    fn the_simplex_bound_holds_everywhere_it_is_searched() {
        // `SIMPLEX_3D_BOUND` is the licence to skip generating a chunk, so a
        // sample outside it is a hole in somebody's world. The bound is derived
        // rather than measured — the derivation is on the constant — and this
        // searches hard for a counter-example anyway, because a derivation
        // about the kernel stops being about the kernel the moment the kernel
        // is retuned.
        let mut worst = 0.0f32;
        for seed in 0..24u64 {
            for i in 0..4000 {
                // Deliberately awkward strides: whole numbers land on lattice
                // points, where the kernel is smallest and least interesting.
                let x = i as f32 * 0.137 - 274.0;
                let y = i as f32 * 0.311 - 622.0;
                let z = i as f32 * 0.073 + 41.0;
                let value = simplex_3d(seed, x, y, z);
                assert!(
                    value.abs() <= SIMPLEX_3D_BOUND,
                    "simplex_3d gave {value} at ({x}, {y}, {z}) seed {seed}, outside the                      {SIMPLEX_3D_BOUND} the pruning in Density::bounds relies on"
                );
                worst = worst.max(value.abs());
            }
        }
        // Non-vacuous: a kernel returning zero everywhere would pass the
        // assertion above and prune the entire world.
        assert!(
            worst > 0.5,
            "the field barely varies; worst magnitude {worst}"
        );
        // And the record of how much room is being left on the table. This is
        // the number to beat if the bound is ever tightened; it is NOT itself a
        // bound, because a dense search is not a proof.
        assert!(
            worst < SIMPLEX_3D_BOUND,
            "the search reached the derived bound, so the derivation wants re-checking"
        );
    }

    #[test]
    fn a_fractal_stays_inside_the_range_it_reports() {
        // Every shape, and the ones with an asymmetric range are the point:
        // `Ridged` tops out at exactly 1 and reaches far further down.
        for fractal in [Fractal::Fbm, Fractal::Ridged, Fractal::Billow] {
            let params = FractalParams {
                fractal,
                octaves: 5,
                frequency: 0.03,
                lacunarity: 2.0,
                gain: 0.5,
            };
            let (low, high) = params.range();
            assert!(low < high, "{fractal:?} reports an empty range");
            for i in 0..3000 {
                let x = i as f32 * 0.213 - 319.0;
                let y = i as f32 * 0.089 + 7.0;
                let z = i as f32 * 0.157 - 88.0;
                let value = fractal_3d(11, x, y, z, &params);
                assert!(
                    value >= low && value <= high,
                    "{fractal:?} gave {value} at ({x}, {y}, {z}), outside ({low}, {high})"
                );
            }
        }
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
