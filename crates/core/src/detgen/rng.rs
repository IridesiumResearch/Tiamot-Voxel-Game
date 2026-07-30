// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Deterministic pseudo-random number generation.
//!
//! Pure integer arithmetic throughout — no floats, so none of the
//! [Deterministic Float Subset](../../../docs/float-determinism.md) hazards
//! apply. Wrapping `u64` arithmetic is exactly specified in Rust and identical
//! on every target.
//!
//! # Why implemented here rather than pulled in
//!
//! Determinism has to be *ours*. A dependency can change its algorithm in a
//! patch release — a legitimate thing for an RNG crate to do, and catastrophic
//! for a world that must regenerate identically five years from now. These are
//! forty lines of well-specified public algorithms; owning them costs almost
//! nothing and removes the risk entirely.
//!
//! # Streams
//!
//! Charter rule 4 requires worldgen randomness to come from
//! `world_seed + chunk_coords + stream_name`. [`StreamRng`] is that: a named,
//! independent stream per purpose. Two generators drawing from `"caves"` and
//! `"ore"` in the same chunk get uncorrelated sequences, and neither shifts if
//! the other changes how many numbers it draws.
//!
//! That last property is what makes streams worth having. Sharing one sequence
//! means adding a single `next()` call to cave generation silently changes
//! every ore placement in the world.

use crate::coords::ChunkPos;

/// `SplitMix64`. Used to expand a single seed into a well-distributed state.
///
/// Three shifts and two multiplies, and it passes `BigCrush`. Its job here is to
/// avoid seeding [`Xoshiro256PlusPlus`] with something structurally poor: an
/// all-zero state is a fixed point, and a state with few set bits takes a long
/// time to escape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplitMix64(u64);

impl SplitMix64 {
    /// Starts from a seed.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// Next value in the sequence.
    pub const fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// xoshiro256++ 1.0.
///
/// Fast, small, and with a 2^256 period. Not cryptographic and not intended to
/// be — worldgen needs reproducibility and good distribution, not unpredictability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Xoshiro256PlusPlus {
    state: [u64; 4],
}

impl Xoshiro256PlusPlus {
    /// Seeds the state via [`SplitMix64`], as the reference implementation
    /// recommends.
    #[must_use]
    pub const fn seed_from_u64(seed: u64) -> Self {
        let mut mix = SplitMix64::new(seed);
        let state = [
            mix.next_u64(),
            mix.next_u64(),
            mix.next_u64(),
            mix.next_u64(),
        ];
        Self { state }
    }

    /// Uses a state directly.
    ///
    /// An all-zero state is a fixed point that only ever produces zero, so it
    /// is replaced with a seeded one rather than silently misbehaving.
    #[must_use]
    pub const fn from_state(state: [u64; 4]) -> Self {
        if state[0] == 0 && state[1] == 0 && state[2] == 0 && state[3] == 0 {
            return Self::seed_from_u64(0);
        }
        Self { state }
    }

    /// Next value in the sequence.
    pub const fn next_u64(&mut self) -> u64 {
        let result = self.state[0]
            .wrapping_add(self.state[3])
            .rotate_left(23)
            .wrapping_add(self.state[0]);

        let t = self.state[1] << 17;
        self.state[2] ^= self.state[0];
        self.state[3] ^= self.state[1];
        self.state[1] ^= self.state[2];
        self.state[0] ^= self.state[3];
        self.state[2] ^= t;
        self.state[3] = self.state[3].rotate_left(45);

        result
    }

    /// Next value in `0..bound`, without modulo bias.
    ///
    /// Lemire's multiply-shift with a rejection loop. The loop rejects with
    /// probability below `bound / 2^64`, so for any realistic bound it is
    /// entered essentially never — but it is what makes the distribution exact,
    /// and an exact distribution is part of being reproducible.
    pub const fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            return 0;
        }
        let threshold = bound.wrapping_neg() % bound;
        loop {
            let candidate = self.next_u64();
            let (high, low) = widening_mul(candidate, bound);
            if low >= threshold {
                return high;
            }
        }
    }

    /// A float in `[0, 1)`.
    ///
    /// Built by taking the top 24 bits — `f32`'s mantissa width — and dividing
    /// by 2^24. Both the cast and the divide are in the allowed subset, and the
    /// division is by a power of two so it is exact.
    ///
    /// Deliberately not `to_bits` tricks: those are also deterministic, but this
    /// is obviously correct and equally fast.
    pub const fn next_f32(&mut self) -> f32 {
        const MANTISSA_BITS: u32 = 24;
        let bits = self.next_u64() >> (64 - MANTISSA_BITS);
        (bits as f32) / ((1u32 << MANTISSA_BITS) as f32)
    }

    /// A float in `[-1, 1)`.
    pub const fn next_f32_signed(&mut self) -> f32 {
        self.next_f32() * 2.0 - 1.0
    }

    /// A `bool` with even odds.
    pub const fn next_bool(&mut self) -> bool {
        // Top bit: the low bits of an xoshiro `++` output are weaker than the
        // high ones.
        self.next_u64() >> 63 == 1
    }
}

/// 64×64→128 multiply, returning `(high, low)`.
const fn widening_mul(a: u64, b: u64) -> (u64, u64) {
    let wide = (a as u128) * (b as u128);
    ((wide >> 64) as u64, wide as u64)
}

/// FNV-1a over a byte string.
///
/// Fixed constants and no seed, so a stream name hashes identically in every
/// process, on every platform, forever. Rust's default hasher is randomly seeded
/// per process and would make stream identity vary run to run — which is the
/// exact failure charter rule 4 warns about.
#[must_use]
pub const fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut index = 0;
    while index < bytes.len() {
        hash ^= bytes[index] as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        index += 1;
    }
    hash
}

/// A named, per-chunk random stream.
///
/// `StreamRng::new(world_seed, chunk_pos, "caves")` gives a sequence that is:
///
/// - **reproducible** — same three inputs, same sequence, forever;
/// - **independent** — a different name decorrelates completely, so drawing
///   more numbers for caves cannot shift ore placement;
/// - **order-free** — chunks can generate in any order, or in parallel, and
///   each gets the same numbers it would have got alone.
///
/// The third property is why generation can be threaded at all without
/// threatening the determinism gate.
#[derive(Debug, Clone)]
pub struct StreamRng {
    inner: Xoshiro256PlusPlus,
}

impl StreamRng {
    /// Opens a named stream for a chunk.
    #[must_use]
    pub fn new(world_seed: u64, chunk: ChunkPos, stream_name: &str) -> Self {
        Self {
            inner: Xoshiro256PlusPlus::seed_from_u64(Self::seed_for(
                world_seed,
                chunk,
                stream_name,
            )),
        }
    }

    /// Opens a named stream not tied to a chunk — for world-level decisions.
    #[must_use]
    pub fn global(world_seed: u64, stream_name: &str) -> Self {
        Self::new(world_seed, ChunkPos::new(0, 0, 0), stream_name)
    }

    /// The derived seed, exposed so a generator can reproduce a stream from its
    /// inputs without holding the stream itself.
    #[must_use]
    pub fn seed_for(world_seed: u64, chunk: ChunkPos, stream_name: &str) -> u64 {
        // Mix each component through SplitMix64 rather than adding or XORing
        // them raw. Nearby chunks differ by 1 in one coordinate, and a raw
        // combination would give nearby chunks visibly related sequences —
        // which shows up as repeating terrain, not as a statistical curiosity.
        let mut mix = SplitMix64::new(world_seed ^ fnv1a(stream_name.as_bytes()));
        let mut seed = mix.next_u64();
        for coordinate in [chunk.x, chunk.y, chunk.z] {
            // Cast through u32 first: `i32 as u64` sign-extends, so -1 and
            // 0xFFFF_FFFF would collide.
            seed ^= SplitMix64::new(seed ^ u64::from(coordinate as u32)).next_u64();
        }
        seed
    }

    /// Next raw value.
    pub const fn next_u64(&mut self) -> u64 {
        self.inner.next_u64()
    }

    /// Next value in `0..bound`, unbiased.
    pub const fn below(&mut self, bound: u64) -> u64 {
        self.inner.below(bound)
    }

    /// Next float in `[0, 1)`.
    pub const fn next_f32(&mut self) -> f32 {
        self.inner.next_f32()
    }

    /// Next float in `[-1, 1)`.
    pub const fn next_f32_signed(&mut self) -> f32 {
        self.inner.next_f32_signed()
    }

    /// Next `bool`.
    pub const fn next_bool(&mut self) -> bool {
        self.inner.next_bool()
    }

    /// True with probability `numerator / denominator`.
    pub const fn chance(&mut self, numerator: u64, denominator: u64) -> bool {
        self.below(denominator) < numerator
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference vectors from an independent implementation of the published
    /// algorithms, not from this code. Two implementations agreeing is evidence;
    /// one implementation agreeing with itself is not.
    #[test]
    fn xoshiro_matches_reference_vectors_from_a_direct_state() {
        let mut rng = Xoshiro256PlusPlus::from_state([1, 2, 3, 4]);
        let expected = [
            0x0280_0001,
            0x0380_0067,
            0x000c_c000_0380_0067,
            0x000c_c201_9944_00b2,
            0x8012_a201_9ac4_33cd,
            0x8a69_978a_cdee_33ba,
            0xc271_1347_3315_4abd,
            0xac2b_a091_7916_9e97,
        ];
        for (index, want) in expected.into_iter().enumerate() {
            assert_eq!(rng.next_u64(), want, "output {index} differs");
        }
    }

    #[test]
    fn splitmix_matches_reference_vectors() {
        let mut mix = SplitMix64::new(0);
        let expected = [
            0xe220_a839_7b1d_cdaf,
            0x6e78_9e6a_a1b9_65f4,
            0x06c4_5d18_8009_454f,
            0xf88b_b8a8_724c_81ec,
            0x1b39_896a_51a8_749b,
            0x53cb_9f0c_747e_a2ea,
            0x2c82_9abe_1f45_32e1,
            0xc584_133a_c916_ab3c,
        ];
        for (index, want) in expected.into_iter().enumerate() {
            assert_eq!(mix.next_u64(), want, "output {index} differs");
        }
    }

    #[test]
    fn seeding_from_splitmix_matches_the_reference_chain() {
        // The composition is what the engine actually uses, so it is what has
        // to match — not just the two pieces separately.
        let mut rng = Xoshiro256PlusPlus::seed_from_u64(0);
        let expected = [
            0x5317_5d61_490b_23df,
            0x61da_6f3d_c380_d507,
            0x5c0f_df91_ec9a_7bfc,
            0x02ee_bf8c_3bbe_5e1a,
            0x7eca_04eb_af4a_5eea,
            0x0543_c377_57f0_8d9a,
            0xdb74_90c7_5ab5_026e,
            0xd873_43e6_464b_c959,
        ];
        for (index, want) in expected.into_iter().enumerate() {
            assert_eq!(rng.next_u64(), want, "output {index} differs");
        }
    }

    #[test]
    fn an_all_zero_state_is_replaced_rather_than_producing_only_zero() {
        let mut rng = Xoshiro256PlusPlus::from_state([0; 4]);
        let values: Vec<u64> = (0..4).map(|_| rng.next_u64()).collect();
        assert!(values.iter().any(|&v| v != 0), "got {values:?}");
    }

    #[test]
    fn streams_are_reproducible() {
        let chunk = ChunkPos::new(3, -7, 11);
        let mut first = StreamRng::new(42, chunk, "caves");
        let mut second = StreamRng::new(42, chunk, "caves");
        for _ in 0..64 {
            assert_eq!(first.next_u64(), second.next_u64());
        }
    }

    #[test]
    fn different_stream_names_decorrelate() {
        // The property that lets one generator change without disturbing
        // another. If these matched, adding a `next()` call to cave generation
        // would silently move every ore in the world.
        let chunk = ChunkPos::new(0, 0, 0);
        let mut caves = StreamRng::new(42, chunk, "caves");
        let mut ore = StreamRng::new(42, chunk, "ore");
        let matches = (0..64)
            .filter(|_| caves.next_u64() == ore.next_u64())
            .count();
        assert_eq!(matches, 0, "streams should not share values");
    }

    #[test]
    fn adjacent_chunks_decorrelate() {
        // Nearby chunks differ by 1 in one coordinate. Mixing each component
        // rather than combining raw is what stops that showing up as visibly
        // repeating terrain.
        let mut seen = std::collections::BTreeSet::new();
        for x in -2..=2 {
            for y in -2..=2 {
                for z in -2..=2 {
                    let mut rng = StreamRng::new(7, ChunkPos::new(x, y, z), "height");
                    assert!(
                        seen.insert(rng.next_u64()),
                        "chunks ({x},{y},{z}) collided with an earlier chunk"
                    );
                }
            }
        }
    }

    #[test]
    fn negative_coordinates_do_not_alias_positive_ones() {
        // `i32 as u64` sign-extends, so -1 would become 0xFFFF_FFFF_FFFF_FFFF
        // and collide with a large positive coordinate if not cast through u32.
        let a = StreamRng::seed_for(1, ChunkPos::new(-1, 0, 0), "s");
        let b = StreamRng::seed_for(1, ChunkPos::new(i32::MAX, 0, 0), "s");
        assert_ne!(a, b);
    }

    #[test]
    fn different_world_seeds_decorrelate() {
        let chunk = ChunkPos::new(1, 2, 3);
        let mut first = StreamRng::new(1, chunk, "s");
        let mut second = StreamRng::new(2, chunk, "s");
        assert_ne!(first.next_u64(), second.next_u64());
    }

    #[test]
    fn below_is_in_range_and_unbiased_enough_to_notice() {
        let mut rng = StreamRng::global(9, "dist");
        let mut buckets = [0u32; 8];
        for _ in 0..80_000 {
            let value = rng.below(8);
            assert!(value < 8);
            buckets[value as usize] += 1;
        }
        // 80,000 draws over 8 buckets is 10,000 each. A 10% band is far wider
        // than sampling noise and far narrower than modulo bias would produce.
        for (bucket, count) in buckets.iter().enumerate() {
            assert!(
                (9_000..11_000).contains(count),
                "bucket {bucket} got {count}, expected ~10000"
            );
        }
    }

    #[test]
    fn below_zero_does_not_loop_forever() {
        let mut rng = StreamRng::global(1, "s");
        assert_eq!(rng.below(0), 0);
    }

    #[test]
    fn floats_are_in_range() {
        let mut rng = StreamRng::global(3, "floats");
        for _ in 0..10_000 {
            let unit = rng.next_f32();
            assert!((0.0..1.0).contains(&unit), "{unit} outside [0, 1)");
            let signed = rng.next_f32_signed();
            assert!((-1.0..1.0).contains(&signed), "{signed} outside [-1, 1)");
        }
    }

    #[test]
    fn floats_are_never_nan_or_infinite() {
        // Charter rule 4 bans NaN in simulation state; the RNG is where a
        // careless implementation would introduce one.
        let mut rng = StreamRng::global(5, "nan");
        for _ in 0..10_000 {
            assert!(rng.next_f32().is_finite());
            assert!(rng.next_f32_signed().is_finite());
        }
    }

    #[test]
    fn fnv1a_is_stable_and_distinguishes_names() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_ne!(fnv1a(b"caves"), fnv1a(b"ore"));
        assert_eq!(fnv1a(b"caves"), fnv1a(b"caves"));
    }
}
