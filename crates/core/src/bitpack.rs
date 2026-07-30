// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! A densely bit-packed array of small unsigned integers.
//!
//! Chunks store one palette index per block. With 4096 blocks per chunk and
//! palettes that are usually tiny, storing those indices as `u16` would waste
//! most of every chunk: a chunk of two materials needs one bit per block, not
//! sixteen. [`BitArray`] stores exactly `bits_per_entry` bits each, packed
//! across `u64` words with entries free to straddle a word boundary.
//!
//! Straddling is deliberate. The alternative — rounding entry width up so a
//! whole number fit per word — wastes up to 20% at widths like 3 and 5, which
//! are precisely the widths a real chunk lands on. The cost is one extra shift
//! and or on the straddling reads, which is cheap next to the cache miss that
//! reading a chunk implies anyway.
//!
//! A width of zero is a real and common case: a chunk of one material needs no
//! index storage at all, and [`BitArray::new`] allocates nothing for it.

/// A fixed-length array of `len` unsigned integers of `bits_per_entry` bits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitArray {
    words: Vec<u64>,
    bits_per_entry: u8,
    len: usize,
}

impl BitArray {
    /// Widest entry this supports. Palette indices are `u16`, so 16 suffices.
    pub const MAX_BITS: u8 = 16;

    /// An array of `len` entries, all zero.
    ///
    /// Allocates nothing when `bits_per_entry` is zero.
    ///
    /// # Panics
    ///
    /// If `bits_per_entry` exceeds [`Self::MAX_BITS`].
    #[must_use]
    pub fn new(len: usize, bits_per_entry: u8) -> Self {
        assert!(
            bits_per_entry <= Self::MAX_BITS,
            "bits_per_entry {bits_per_entry} exceeds {}",
            Self::MAX_BITS
        );
        let words = vec![0; Self::words_needed(len, bits_per_entry)];
        Self {
            words,
            bits_per_entry,
            len,
        }
    }

    /// How many `u64` words `len` entries of the given width occupy.
    #[must_use]
    pub const fn words_needed(len: usize, bits_per_entry: u8) -> usize {
        if bits_per_entry == 0 {
            return 0;
        }
        let total_bits = len * bits_per_entry as usize;
        total_bits.div_ceil(64)
    }

    /// Smallest entry width that can address `values` distinct values.
    ///
    /// Zero for one value or none — a single-entry palette needs no index at
    /// all, since every lookup answers 0.
    #[must_use]
    pub fn bits_for(values: usize) -> u8 {
        if values <= 1 {
            0
        } else {
            // Number of bits needed to represent `values - 1`.
            (usize::BITS - (values - 1).leading_zeros()) as u8
        }
    }

    /// Number of entries.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether there are no entries.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Bits per entry.
    #[must_use]
    pub const fn bits_per_entry(&self) -> u8 {
        self.bits_per_entry
    }

    /// Heap bytes occupied by the packed words.
    #[must_use]
    pub fn memory_usage(&self) -> usize {
        self.words.capacity() * size_of::<u64>()
    }

    /// The entry at `index`.
    ///
    /// # Panics
    ///
    /// If `index` is out of bounds.
    #[must_use]
    pub fn get(&self, index: usize) -> u32 {
        assert!(
            index < self.len,
            "index {index} out of bounds ({})",
            self.len
        );
        if self.bits_per_entry == 0 {
            return 0;
        }

        let bits = self.bits_per_entry as usize;
        let bit_offset = index * bits;
        let word = bit_offset / 64;
        let shift = bit_offset % 64;
        let mask = Self::entry_mask(self.bits_per_entry);

        let mut value = (self.words[word] >> shift) & mask;
        // Straddles into the next word when the entry does not fit in what
        // remains of this one.
        if shift + bits > 64 {
            value |= (self.words[word + 1] << (64 - shift)) & mask;
        }
        value as u32
    }

    /// Sets the entry at `index`.
    ///
    /// # Panics
    ///
    /// If `index` is out of bounds, or if `value` does not fit in
    /// `bits_per_entry` bits.
    pub fn set(&mut self, index: usize, value: u32) {
        assert!(
            index < self.len,
            "index {index} out of bounds ({})",
            self.len
        );
        let mask = Self::entry_mask(self.bits_per_entry);
        assert!(
            u64::from(value) <= mask,
            "value {value} does not fit in {} bits",
            self.bits_per_entry
        );
        if self.bits_per_entry == 0 {
            return;
        }

        let bits = self.bits_per_entry as usize;
        let bit_offset = index * bits;
        let word = bit_offset / 64;
        let shift = bit_offset % 64;
        let value = u64::from(value);

        self.words[word] &= !(mask << shift);
        self.words[word] |= value << shift;

        if shift + bits > 64 {
            let carried = 64 - shift;
            self.words[word + 1] &= !(mask >> carried);
            self.words[word + 1] |= value >> carried;
        }
    }

    /// Returns a copy re-encoded at a new entry width, preserving every value.
    ///
    /// # Panics
    ///
    /// If any stored value does not fit in `bits_per_entry` bits, or if
    /// `bits_per_entry` exceeds [`Self::MAX_BITS`].
    #[must_use]
    pub fn resized(&self, bits_per_entry: u8) -> Self {
        let mut resized = Self::new(self.len, bits_per_entry);
        if bits_per_entry > 0 {
            for index in 0..self.len {
                resized.set(index, self.get(index));
            }
        }
        resized
    }

    /// Rewrites every entry through `remap`.
    ///
    /// # Panics
    ///
    /// If `remap` returns a value too large for the entry width.
    pub fn remap(&mut self, remap: impl Fn(u32) -> u32) {
        if self.bits_per_entry == 0 {
            return;
        }
        for index in 0..self.len {
            let value = self.get(index);
            self.set(index, remap(value));
        }
    }

    /// Iterates every entry in order.
    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        (0..self.len).map(|index| self.get(index))
    }

    const fn entry_mask(bits_per_entry: u8) -> u64 {
        if bits_per_entry == 0 {
            0
        } else {
            u64::MAX >> (64 - bits_per_entry as u32)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_for_matches_palette_growth_thresholds() {
        // The thresholds the chunk design specifies: 1 entry needs no index
        // storage, up to 2 needs 1 bit, up to 4 needs 2, and so on.
        assert_eq!(BitArray::bits_for(0), 0);
        assert_eq!(BitArray::bits_for(1), 0);
        assert_eq!(BitArray::bits_for(2), 1);
        assert_eq!(BitArray::bits_for(3), 2);
        assert_eq!(BitArray::bits_for(4), 2);
        assert_eq!(BitArray::bits_for(5), 3);
        assert_eq!(BitArray::bits_for(8), 3);
        assert_eq!(BitArray::bits_for(9), 4);
        assert_eq!(BitArray::bits_for(256), 8);
        assert_eq!(BitArray::bits_for(257), 9);
        assert_eq!(BitArray::bits_for(65_536), 16);
    }

    #[test]
    fn zero_width_allocates_nothing_and_reads_zero() {
        let array = BitArray::new(4096, 0);
        assert_eq!(array.memory_usage(), 0);
        assert_eq!(array.get(0), 0);
        assert_eq!(array.get(4095), 0);
    }

    #[test]
    fn round_trips_at_every_supported_width() {
        for bits in 1..=BitArray::MAX_BITS {
            let max = (1u32 << bits) - 1;
            let len = 200;
            let mut array = BitArray::new(len, bits);
            // A pattern that exercises the full value range and every possible
            // alignment against the 64-bit word boundary.
            for index in 0..len {
                array.set(index, (index as u32 * 7 + 1) % (max + 1));
            }
            for index in 0..len {
                assert_eq!(
                    array.get(index),
                    (index as u32 * 7 + 1) % (max + 1),
                    "width {bits}, index {index}"
                );
            }
        }
    }

    #[test]
    fn entries_straddling_a_word_boundary_survive() {
        // Width 3 divides 64 unevenly, so entry 21 spans bits 63..66 — the
        // exact case naive packing gets wrong.
        let mut array = BitArray::new(64, 3);
        array.set(21, 0b101);
        assert_eq!(array.get(21), 0b101);

        // And the neighbours must be untouched.
        assert_eq!(array.get(20), 0);
        assert_eq!(array.get(22), 0);

        array.set(20, 0b111);
        array.set(22, 0b111);
        assert_eq!(array.get(21), 0b101, "neighbour writes corrupted the entry");
    }

    #[test]
    fn writing_one_entry_never_disturbs_another() {
        let mut array = BitArray::new(100, 5);
        for index in 0..100 {
            array.set(index, 31);
        }
        array.set(50, 0);
        for index in 0..100 {
            let expected = if index == 50 { 0 } else { 31 };
            assert_eq!(array.get(index), expected, "at {index}");
        }
    }

    #[test]
    fn resizing_preserves_every_value() {
        let mut array = BitArray::new(300, 4);
        for index in 0..300 {
            array.set(index, (index % 16) as u32);
        }

        let widened = array.resized(9);
        assert_eq!(widened.bits_per_entry(), 9);
        for index in 0..300 {
            assert_eq!(widened.get(index), (index % 16) as u32, "widen at {index}");
        }

        let narrowed = widened.resized(4);
        assert_eq!(narrowed, array, "widen then narrow should be identity");
    }

    #[test]
    fn resizing_from_zero_width_yields_all_zeroes() {
        let array = BitArray::new(10, 0);
        let widened = array.resized(4);
        assert!(widened.iter().all(|value| value == 0));
    }

    #[test]
    fn remap_rewrites_every_entry() {
        let mut array = BitArray::new(10, 4);
        for index in 0..10 {
            array.set(index, index as u32);
        }
        array.remap(|value| 9 - value);
        let values: Vec<_> = array.iter().collect();
        assert_eq!(values, vec![9, 8, 7, 6, 5, 4, 3, 2, 1, 0]);
    }

    #[test]
    fn words_needed_rounds_up() {
        assert_eq!(BitArray::words_needed(4096, 0), 0);
        assert_eq!(BitArray::words_needed(64, 1), 1);
        assert_eq!(BitArray::words_needed(65, 1), 2);
        assert_eq!(BitArray::words_needed(4096, 4), 256);
        // 4096 entries at 3 bits is 12,288 bits = 192 words exactly.
        assert_eq!(BitArray::words_needed(4096, 3), 192);
    }

    #[test]
    #[should_panic(expected = "does not fit")]
    fn setting_an_oversized_value_panics() {
        let mut array = BitArray::new(4, 2);
        array.set(0, 4);
    }
}
