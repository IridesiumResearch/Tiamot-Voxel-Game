// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Deterministic generation primitives: noise, random streams, and the guards
//! that keep them bit-identical across platforms.
//!
//! [`docs/float-determinism.md`](../../../docs/float-determinism.md) is the
//! authority on what code here may and may not do. The short version: use
//! `f32`, restrict which operations you use, and never reach for fixed-point.
//! Rust guarantees `+ - * / %`, `sqrt`, `abs`, `copysign`, and comparisons match
//! IEEE 754-2008 exactly and has no fast-math mode, so the same sequence of
//! allowed operations gives the same bits everywhere.
//!
//! # These are mechanisms, not policy
//!
//! There is no terrain here. No biomes, no ore distribution, no tree placement,
//! no material names. Worldgen *policy* is Lua mods (charter rule 1); this
//! module gives them noise, streams, and a buffer to write into. The only
//! material-shaped things in this file are test fixtures, and they are named
//! `FIXTURE_*` so that a grep for terrain vocabulary comes back empty.
//!
//! # Layout
//!
//! - [`rng`] — `SplitMix64`, xoshiro256++, and named per-chunk streams
//! - [`noise`] — gradient noise on two lattices, fBm/ridged/billow, bulk fills
//! - [`buffer`] — [`ChunkBuffer`], block-resolution by default
//! - [`assert_ieee_mode`] — the flush-to-zero guard
//! - [`fingerprint`] — the cross-platform hash gate

pub mod buffer;
pub mod noise;
pub mod rng;
pub mod trig;

pub use buffer::{BufferError, ChunkBuffer};
pub use noise::{
    Fractal, FractalParams, Region2d, Region3d, fill_2d, fill_3d, fractal_2d, fractal_3d,
};
pub use rng::{StreamRng, Xoshiro256PlusPlus, fnv1a};

use crate::coords::ChunkPos;

/// Floor as an `i32`, without touching libm.
///
/// `f32::floor` is banned, and the reason is worth restating because it is not
/// the obvious one: `floor` *is* an exactly-specified IEEE 754 operation, but
/// the instruction that implements it in one step (`roundss`) is **`SSE4.1`**,
/// and the supported `x86_64` baseline is **`SSE2`**. On an `SSE2`-only target LLVM
/// cannot emit it, so `f32::floor` becomes a call into platform libm — with
/// nothing in the source to suggest anything changed.
///
/// This does the same job with a truncating cast and a comparison. Rust's
/// float-to-int casts are saturating and fully defined, so this is total: it
/// cannot trap, wrap, or produce UB for any input including NaN and infinity.
///
/// It is also faster than a libm call.
#[must_use]
pub fn floor_to_i32(x: f32) -> i32 {
    // `as` truncates toward zero, so for negative non-integers the result is one
    // too high. The comparison corrects exactly those cases.
    //
    // `saturating_sub` rather than `-`: a value at or below `i32::MIN` saturates
    // to `i32::MIN`, and subtracting one from that overflows. Reachable from
    // `f32::MIN`, and a debug-build panic deep in a noise inner loop is a poor
    // way to find out.
    let truncated = x as i32;
    truncated.saturating_sub(i32::from(x < truncated as f32))
}

/// [`floor_to_i32`] in double precision.
///
/// The same reasoning applies unchanged, and for the same instruction: the
/// one-step `f64` floor is `roundsd`, which is also `SSE4.1`, so `f64::floor` on
/// the `SSE2` baseline is also a libm call with nothing in the source to say so.
///
/// Wanted wherever two coordinate frames have to be compared in absolute world
/// cells — see [`crate::place::blocks_a_body`]. `i64` because a 120,000-block
/// world is 360,000 cells across and the arithmetic that gets there should not
/// have to think about where `i32` ends.
#[must_use]
pub fn floor_to_i64(x: f64) -> i64 {
    let truncated = x as i64;
    truncated.saturating_sub(i64::from(x < truncated as f64))
}

/// The subnormal probe input: the smallest positive **normal** `f32`.
const FTZ_PROBE_INPUT: u32 = 0x0080_0000;

/// What halving it must produce: the largest subnormal, `0x0040_0000`.
///
/// Under flush-to-zero this becomes `0x0000_0000` instead. That is the entire
/// test.
const FTZ_PROBE_EXPECTED: u32 = 0x0040_0000;

/// The CPU's floating-point mode is not IEEE-conforming.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "this thread's FPU is in flush-to-zero or denormals-are-zero mode: halving the smallest \
     normal f32 gave {found:#010x}, expected {FTZ_PROBE_EXPECTED:#010x}. Subnormal results are \
     being silently replaced with zero, so simulation on this thread will NOT match other \
     machines. Audio backends and GPU drivers set this mode process-wide; simulation must \
     never run on a thread owned by one. See docs/float-determinism.md."
)]
pub struct NotIeeeMode {
    /// The bit pattern the probe actually produced.
    pub found: u32,
}

/// Checks that this thread's FPU is in IEEE-conforming mode.
///
/// Flush-to-zero (FTZ) and denormals-are-zero (DAZ) are **thread-local CPU
/// state** — `MXCSR` on x86, `FPCR.FZ` on ARM — not properties of the program.
/// With them set, subnormal results are silently replaced by zero, and identical
/// code on identical input produces different bits on different machines.
///
/// Audio backends and GPU drivers set them process-wide. This is a reasonable
/// thing for them to do and a catastrophic thing for a simulation to inherit.
///
/// # Errors
///
/// [`NotIeeeMode`] if the probe is flushed to zero.
pub fn check_ieee_mode() -> Result<(), NotIeeeMode> {
    // black_box on the INPUT is what makes this a runtime check. Without it the
    // compiler folds the whole expression at compile time and reports its own
    // answer — which is always correct, and therefore always useless.
    let input = std::hint::black_box(f32::from_bits(FTZ_PROBE_INPUT));
    let found = (input / 2.0).to_bits();
    if found == FTZ_PROBE_EXPECTED {
        Ok(())
    } else {
        Err(NotIeeeMode { found })
    }
}

/// Asserts this thread's FPU is IEEE-conforming, panicking with a diagnostic if
/// not.
///
/// **Call this at the top of every simulation thread**, before any simulation
/// work. Failing loudly at spawn is far better than a world that quietly
/// generates differently from everyone else's.
///
/// # Panics
///
/// If the thread is in FTZ or DAZ mode.
pub fn assert_ieee_mode() {
    if let Err(err) = check_ieee_mode() {
        panic!("{err}");
    }
}

/// Fixture materials for the fingerprint recipe.
///
/// Numeric ids with no names and no meaning. The fingerprint hashes *material
/// ids*, so it needs some — but giving them names would put terrain vocabulary
/// in a module that is supposed to contain none.
const FIXTURE_SOLID: crate::material::MaterialId = crate::material::MaterialId(2);

/// The fixed noise recipe the fingerprint uses.
///
/// **Changing any of these numbers changes every golden hash.** That is the
/// point: the fingerprint is a canary for accidental changes to the generation
/// path, so it must be sensitive. If a deliberate change makes it necessary,
/// regenerate the goldens in the same commit and say so in the message.
const FINGERPRINT_PARAMS: noise::FractalParams = noise::FractalParams {
    fractal: noise::Fractal::Fbm,
    octaves: 4,
    frequency: 0.031_25,
    lacunarity: 2.0,
    gain: 0.5,
};

/// Hashes a chunk's worth of generated content into one `u64`.
///
/// **This is the cross-platform determinism gate.** The CI matrix runs the
/// golden-hash test on Linux, Windows, and macOS; one differing bit on any of
/// them fails the build.
///
/// The recipe deliberately exercises the whole path rather than just the noise:
/// a 2D fBm fill drives a heightmap, a random stream salts it, the result goes
/// through [`ChunkBuffer`] and [`ChunkBuffer::to_chunk`], and the hash runs over
/// the resulting block materials. A change anywhere in that chain moves the
/// number.
///
/// # Panics
///
/// Never in practice: the two fallible calls inside are given buffers sized from
/// the very constants that define their expected size, so a failure would mean
/// the chunk dimensions changed underneath a compile-time array. They are
/// `expect`ed rather than propagated because a `Result` here would put an error
/// case in every caller for something that cannot occur.
#[must_use]
pub fn fingerprint(world_seed: u64, chunk: ChunkPos) -> u64 {
    let mut heights = [0i32; (crate::CHUNK_BLOCKS * crate::CHUNK_BLOCKS) as usize];
    let region = noise::Region2d {
        origin_x: (chunk.x * crate::CHUNK_BLOCKS as i32) as f32,
        origin_y: (chunk.z * crate::CHUNK_BLOCKS as i32) as f32,
        step_x: 1.0,
        step_y: 1.0,
        width: crate::CHUNK_BLOCKS as usize,
        height: crate::CHUNK_BLOCKS as usize,
    };

    let mut samples = vec![0.0f32; region.len()];
    noise::fill_2d(world_seed, &region, &FINGERPRINT_PARAMS, &mut samples)
        .expect("the buffer is sized from the region it is filled from");

    // Salt with a named stream, so the RNG is in the hashed path too.
    let mut stream = rng::StreamRng::new(world_seed, chunk, "detgen:fingerprint");

    // Heights are expressed RELATIVE to this chunk's own vertical range, so the
    // surface always lands inside it. An absolute height would put the surface
    // above most chunks, every column would clamp to "full", and the hash would
    // be identical for every position — a gate that passes on everything.
    let base_y = chunk.y * crate::CHUNK_BLOCKS as i32;
    let mid = crate::CHUNK_BLOCKS as i32 / 2;
    for (index, sample) in samples.iter().enumerate() {
        let salt = (stream.below(3) as i32) - 1;
        // Amplitude 6 keeps the surface clear of both the floor and the ceiling,
        // so the noise is what varies the result rather than the clamp.
        heights[index] = base_y + mid + (sample * 6.0) as i32 + salt;
    }

    let mut buffer = ChunkBuffer::air(chunk);
    buffer
        .fill_below_heightmap(&heights, FIXTURE_SOLID)
        .expect("the heightmap is sized from the chunk");

    // A single chiselled block, so the sub-node path is inside the gate too —
    // otherwise a regression in expansion or canonicalisation would not move
    // the hash.
    buffer.set_subnode(
        crate::coords::LocalBlock::new(3, 1, 5),
        1,
        1,
        1,
        crate::material::MaterialId::AIR,
    );

    let built = buffer.to_chunk();

    // A 3D sample set folded in as well.
    //
    // Without this the gate would not cover `simplex_3d` at all: the heightmap
    // path is entirely 2D, so a 3D regression could ship with every golden hash
    // still matching. Found by changing the 3D gradient table and watching the
    // hashes not move.
    //
    // Sixty-four samples rather than a full 48³ fill — enough to catch a changed
    // field, cheap enough that the gate stays fast on three CI legs.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for step in 0..64u32 {
        let t = step as f32;
        let sample = noise::fractal_3d(
            world_seed,
            t * 0.5 + chunk.x as f32,
            t * 0.25 + chunk.y as f32,
            t * 0.125 + chunk.z as f32,
            &FINGERPRINT_PARAMS,
        );
        // Hash the BIT PATTERN, not a rounded value: the gate exists to catch a
        // last-bit difference, and rounding first would hide exactly that.
        for byte in sample.to_bits().to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    // FNV-1a over the resulting materials, continuing the same hash. Fixed
    // constants, no seed, fixed iteration order — the hash itself must not be a
    // source of variation.
    for index in 0..crate::BLOCKS_PER_CHUNK {
        let view = built.get_block_local(crate::coords::LocalBlock::from_index(index));
        for cell in view.cells() {
            for byte in cell.get().to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_matches_the_mathematical_definition() {
        // Including the negative cases a truncating cast gets wrong.
        let cases = [
            (0.0, 0),
            (0.5, 0),
            (1.0, 1),
            (1.9, 1),
            (-0.0, 0),
            (-0.5, -1),
            (-1.0, -1),
            (-1.1, -2),
            (-2.9, -3),
            (100.999, 100),
            (-100.001, -101),
        ];
        for (input, expected) in cases {
            assert_eq!(floor_to_i32(input), expected, "floor({input})");
        }
    }

    #[test]
    fn floor_is_total_on_hostile_input() {
        // Rust's float-to-int casts saturate, so none of these may panic.
        for input in [
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NAN,
            f32::MAX,
            f32::MIN,
        ] {
            let _ = floor_to_i32(input);
        }
    }

    #[test]
    fn the_ieee_mode_guard_passes_in_a_normal_process() {
        assert!(
            check_ieee_mode().is_ok(),
            "the test process should be IEEE-conforming"
        );
        assert_ieee_mode();
    }

    /// Sets or clears flush-to-zero and denormals-are-zero on this thread.
    ///
    /// Inline assembly rather than `_mm_setcsr`, which is deprecated. Restoring
    /// the previous value is the caller's job, and the test below does it before
    /// asserting anything that could unwind.
    #[cfg(target_arch = "x86_64")]
    unsafe fn set_ftz_daz(enable: bool) -> u32 {
        let mut csr: u32 = 0;
        unsafe {
            std::arch::asm!("stmxcsr [{}]", in(reg) &raw mut csr, options(nostack));
        }
        let previous = csr;
        // Bit 15 = FTZ, bit 6 = DAZ.
        let mut updated = csr;
        if enable {
            updated |= (1 << 15) | (1 << 6);
        } else {
            updated &= !((1 << 15) | (1 << 6));
        }
        unsafe {
            std::arch::asm!("ldmxcsr [{}]", in(reg) &raw const updated, options(nostack));
        }
        previous
    }

    #[cfg(target_arch = "x86_64")]
    unsafe fn restore_mxcsr(value: u32) {
        unsafe {
            std::arch::asm!("ldmxcsr [{}]", in(reg) &raw const value, options(nostack));
        }
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn the_guard_catches_deliberately_enabled_flush_to_zero() {
        // The test that makes the guard worth having: without it, this whole
        // mechanism could be broken and every other test would still pass.
        let previous = unsafe { set_ftz_daz(true) };
        let result = check_ieee_mode();
        // Restore BEFORE asserting — an assertion failure unwinds, and leaving
        // the thread in FTZ would poison every later test on it.
        unsafe { restore_mxcsr(previous) };

        let err = result.expect_err("the guard must notice flush-to-zero");
        assert_eq!(err.found, 0, "under FTZ the subnormal result is zeroed");
        assert!(
            err.to_string().contains("flush-to-zero"),
            "the diagnostic must name the cause: {err}"
        );
        assert!(
            err.to_string().contains("Audio backends and GPU drivers"),
            "the diagnostic must name the likely culprit: {err}"
        );

        // And the guard must pass again now that the mode is restored.
        assert!(check_ieee_mode().is_ok());
    }

    #[test]
    #[cfg(target_arch = "aarch64")]
    fn the_guard_catches_deliberately_enabled_flush_to_zero() {
        // FPCR bit 24 is FZ.
        let previous: u64;
        unsafe {
            std::arch::asm!("mrs {}, fpcr", out(reg) previous, options(nostack));
            let enabled = previous | (1 << 24);
            std::arch::asm!("msr fpcr, {}", in(reg) enabled, options(nostack));
        }
        let result = check_ieee_mode();
        unsafe {
            std::arch::asm!("msr fpcr, {}", in(reg) previous, options(nostack));
        }

        let err = result.expect_err("the guard must notice flush-to-zero");
        assert_eq!(err.found, 0);
        assert!(check_ieee_mode().is_ok());
    }

    #[test]
    fn the_fingerprint_is_reproducible_within_a_process() {
        let chunk = ChunkPos::new(1, 0, -3);
        assert_eq!(fingerprint(1234, chunk), fingerprint(1234, chunk));
    }

    #[test]
    fn the_fingerprint_responds_to_seed_and_position() {
        let chunk = ChunkPos::new(0, 0, 0);
        assert_ne!(fingerprint(1, chunk), fingerprint(2, chunk));
        assert_ne!(
            fingerprint(1, chunk),
            fingerprint(1, ChunkPos::new(1, 0, 0))
        );
        assert_ne!(
            fingerprint(1, chunk),
            fingerprint(1, ChunkPos::new(0, 0, 1))
        );
    }
}
