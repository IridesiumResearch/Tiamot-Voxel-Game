// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Packing a chunk's light for the wire.
//!
//! # Why run-length and not zstd
//!
//! Light is the most run-heavy data in the engine. An open-sky chunk is 4,096
//! copies of one value, a chunk underground is 4,096 copies of another, and a
//! chunk with a cave in it is a handful of long runs. Run-length encoding takes
//! the first two to four bytes and costs no compressor — chunk blobs already pay
//! for zstd, and paying again for a field that is usually one value would be
//! spending milliseconds to save nothing.
//!
//! # This decodes hostile input
//!
//! **Charter rule 14.** A client decodes this from a server it does not trust.
//! Every bound is checked before anything is allocated: the run lengths are
//! summed against [`crate::BLOCKS_PER_CHUNK`] as they are read, so a header
//! claiming four billion blocks is refused at the third byte rather than after
//! an 8 GiB allocation. The decoder allocates exactly one fixed-size layer and
//! never grows it.

use crate::BLOCKS_PER_CHUNK;
use crate::coords::LocalBlock;

use super::{Light, LightLayer};

/// A layer where every block is the same level.
const TAG_UNIFORM: u8 = 0;

/// Runs of `(count: u16, level: u16)`, little-endian.
const TAG_RUNS: u8 = 1;

/// Why a light payload would not decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LightDecodeError {
    /// The payload was empty, so there is not even a tag.
    #[error("a light payload needs at least a tag byte")]
    Empty,

    /// The first byte named an encoding this build does not have.
    #[error("unknown light encoding {tag}")]
    UnknownTag {
        /// What the payload claimed.
        tag: u8,
    },

    /// A run header or value was cut short.
    #[error("a light payload ended in the middle of a run")]
    Truncated,

    /// A run of zero blocks, which cannot be encoded and would let a payload
    /// carry unbounded runs without ever covering the chunk.
    #[error("a light run covers no blocks")]
    EmptyRun,

    /// The runs cover more or fewer blocks than a chunk has.
    #[error("light runs cover {covered} blocks; a chunk has {expected}")]
    WrongLength {
        /// What the payload described.
        covered: usize,
        /// What it had to describe.
        expected: usize,
    },
}

/// Packs a layer for the wire.
#[must_use]
pub fn encode(layer: &LightLayer) -> Vec<u8> {
    if let Some(level) = layer.is_uniform() {
        let mut out = Vec::with_capacity(3);
        out.push(TAG_UNIFORM);
        out.extend_from_slice(&level.0.to_le_bytes());
        return out;
    }

    let levels = layer.levels();
    let mut out = vec![TAG_RUNS];
    let mut index = 0;
    while index < levels.len() {
        let level = levels[index];
        let mut run = 1;
        // A run length is a `u16` and a chunk has 4,096 blocks, so no run can
        // overflow the field. The cap is written out anyway rather than left as
        // an invariant of a constant somebody may change.
        while index + run < levels.len() && levels[index + run] == level && run < u16::MAX as usize
        {
            run += 1;
        }
        out.extend_from_slice(&(run as u16).to_le_bytes());
        out.extend_from_slice(&level.0.to_le_bytes());
        index += run;
    }
    out
}

/// Unpacks a layer received from a peer.
///
/// # Errors
///
/// [`LightDecodeError`] for anything that is not exactly one chunk's worth of
/// well-formed runs. Nothing is allocated beyond one layer, whatever the
/// payload claims.
pub fn decode(bytes: &[u8]) -> Result<LightLayer, LightDecodeError> {
    let (&tag, rest) = bytes.split_first().ok_or(LightDecodeError::Empty)?;

    match tag {
        TAG_UNIFORM => {
            let level = read_u16(rest, 0).ok_or(LightDecodeError::Truncated)?;
            if rest.len() != 2 {
                return Err(LightDecodeError::Truncated);
            }
            Ok(LightLayer::uniform(Light(level)))
        }

        TAG_RUNS => {
            // Built as uniform and promoted by the first differing write, so a
            // payload that turns out to be uniform after all costs one word
            // rather than a dense array nobody needed.
            let mut layer = LightLayer::dark();
            let mut covered = 0usize;
            let mut offset = 0usize;

            while offset < rest.len() {
                let count = read_u16(rest, offset).ok_or(LightDecodeError::Truncated)? as usize;
                let level = read_u16(rest, offset + 2).ok_or(LightDecodeError::Truncated)?;
                offset += 4;

                if count == 0 {
                    return Err(LightDecodeError::EmptyRun);
                }
                // Checked BEFORE writing, so a payload cannot walk past the end
                // of the chunk one run at a time.
                if covered + count > BLOCKS_PER_CHUNK {
                    return Err(LightDecodeError::WrongLength {
                        covered: covered + count,
                        expected: BLOCKS_PER_CHUNK,
                    });
                }

                let level = Light(level);
                for index in covered..covered + count {
                    layer.set(LocalBlock::from_index(index), level);
                }
                covered += count;
            }

            if covered != BLOCKS_PER_CHUNK {
                return Err(LightDecodeError::WrongLength {
                    covered,
                    expected: BLOCKS_PER_CHUNK,
                });
            }
            layer.compact();
            Ok(layer)
        }

        tag => Err(LightDecodeError::UnknownTag { tag }),
    }
}

/// A little-endian `u16` at `offset`, or `None` if it is not fully there.
fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let end = offset.checked_add(2)?;
    let slice = bytes.get(offset..end)?;
    Some(u16::from_le_bytes([slice[0], slice[1]]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::light::MAX_LEVEL;

    fn at(index: usize) -> LocalBlock {
        LocalBlock::from_index(index)
    }

    #[test]
    fn a_uniform_layer_costs_three_bytes() {
        // The common case by a wide margin: open sky, or anything underground.
        let layer = LightLayer::uniform(Light::DAYLIGHT);
        let bytes = encode(&layer);
        assert_eq!(
            bytes.len(),
            3,
            "a uniform chunk should be a tag and a value"
        );
        assert_eq!(decode(&bytes).expect("decode"), layer);
    }

    #[test]
    fn a_mixed_layer_round_trips() {
        let mut layer = LightLayer::dark();
        for index in 0..BLOCKS_PER_CHUNK {
            layer.set(at(index), Light::new((index % 16) as u8, 0, 0, 0));
        }
        let decoded = decode(&encode(&layer)).expect("decode");
        for index in 0..BLOCKS_PER_CHUNK {
            assert_eq!(
                decoded.get(at(index)),
                layer.get(at(index)),
                "block {index} came back wrong"
            );
        }
    }

    #[test]
    fn a_layer_that_became_uniform_decodes_back_to_one_word() {
        // A dense layer whose values all agree encodes as runs and must still
        // decode compactly — otherwise a chunk that was relit to a single level
        // keeps 8 KiB on the client for ever.
        let mut layer = LightLayer::dark();
        for index in 0..BLOCKS_PER_CHUNK {
            layer.set(at(index), Light::DAYLIGHT);
        }
        let decoded = decode(&encode(&layer)).expect("decode");
        assert!(decoded.is_compact(), "a uniform payload decoded dense");
    }

    #[test]
    fn a_long_run_survives_the_u16_length_field() {
        // 4,096 fits in a u16 with room to spare, but the encoder's cap is
        // written out rather than assumed — this pins that a whole-chunk run is
        // one run and not a truncated one.
        let layer = LightLayer::uniform(Light::new(3, 4, 5, 6));
        let mut dense = LightLayer::dark();
        for index in 0..BLOCKS_PER_CHUNK {
            dense.set(at(index), Light::new(3, 4, 5, 6));
        }
        assert_eq!(decode(&encode(&dense)).expect("decode"), layer);
    }

    // -- hostile input, charter rule 14 ----------------------------------

    #[test]
    fn an_empty_payload_is_refused() {
        assert_eq!(decode(&[]), Err(LightDecodeError::Empty));
    }

    #[test]
    fn an_unknown_tag_is_refused_by_name() {
        assert_eq!(decode(&[9]), Err(LightDecodeError::UnknownTag { tag: 9 }));
    }

    #[test]
    fn a_truncated_uniform_payload_is_refused() {
        assert_eq!(decode(&[TAG_UNIFORM, 1]), Err(LightDecodeError::Truncated));
        // And trailing bytes are refused too: a payload that decodes but has
        // more in it than it should is a peer that disagrees about the format.
        assert_eq!(
            decode(&[TAG_UNIFORM, 1, 0, 0]),
            Err(LightDecodeError::Truncated)
        );
    }

    #[test]
    fn a_run_claiming_more_than_a_chunk_is_refused_before_it_writes() {
        // **The allocation guard.** A single run of 65,535 would otherwise
        // write past the end of a chunk sixteen times over.
        let mut bytes = vec![TAG_RUNS];
        bytes.extend_from_slice(&u16::MAX.to_le_bytes());
        bytes.extend_from_slice(&Light::DAYLIGHT.0.to_le_bytes());
        assert_eq!(
            decode(&bytes),
            Err(LightDecodeError::WrongLength {
                covered: u16::MAX as usize,
                expected: BLOCKS_PER_CHUNK,
            })
        );
    }

    #[test]
    fn many_small_runs_cannot_walk_past_the_end_either() {
        // The same guard from the other direction: a thousand short runs must
        // be refused at the one that crosses the boundary, not after it.
        let mut bytes = vec![TAG_RUNS];
        for _ in 0..=(BLOCKS_PER_CHUNK / 16) {
            bytes.extend_from_slice(&16u16.to_le_bytes());
            bytes.extend_from_slice(&Light::DAYLIGHT.0.to_le_bytes());
        }
        assert!(matches!(
            decode(&bytes),
            Err(LightDecodeError::WrongLength { .. })
        ));
    }

    #[test]
    fn a_short_payload_is_refused_rather_than_padded() {
        // Covering fewer blocks than a chunk has would leave the rest at
        // whatever the decoder happened to start with, which is a silent
        // half-lit chunk rather than an error.
        let mut bytes = vec![TAG_RUNS];
        bytes.extend_from_slice(&8u16.to_le_bytes());
        bytes.extend_from_slice(&Light::DAYLIGHT.0.to_le_bytes());
        assert_eq!(
            decode(&bytes),
            Err(LightDecodeError::WrongLength {
                covered: 8,
                expected: BLOCKS_PER_CHUNK,
            })
        );
    }

    #[test]
    fn a_zero_length_run_is_refused() {
        // Without this a payload could carry unlimited runs while never
        // covering the chunk, which is a decoder that spins on attacker input.
        let mut bytes = vec![TAG_RUNS];
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&Light::DAYLIGHT.0.to_le_bytes());
        assert_eq!(decode(&bytes), Err(LightDecodeError::EmptyRun));
    }

    #[test]
    fn a_run_cut_off_mid_header_is_refused() {
        let mut bytes = vec![TAG_RUNS];
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.push(0); // half a level
        assert_eq!(decode(&bytes), Err(LightDecodeError::Truncated));
    }

    #[test]
    fn every_level_value_survives_a_round_trip() {
        // Including levels no propagation would produce: the decoder must not
        // assume the sender's rules, only the format's.
        let mut layer = LightLayer::dark();
        for index in 0..BLOCKS_PER_CHUNK {
            layer.set(at(index), Light(index as u16));
        }
        let decoded = decode(&encode(&layer)).expect("decode");
        for index in 0..BLOCKS_PER_CHUNK {
            assert_eq!(decoded.get(at(index)).0, index as u16);
        }
        let _ = MAX_LEVEL;
    }
}
