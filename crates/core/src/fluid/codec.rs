// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Packing a chunk's fluid for the wire.
//!
//! # Why run-length, and why an empty tag
//!
//! The same reasoning as [`crate::light::codec`] — fluid is run-heavy, and a
//! compressor would cost milliseconds to save nothing on a field that is
//! usually one value. Fluid goes further in one respect: the overwhelmingly
//! common chunk holds **no fluid at all**, so that case gets a tag of its own
//! and encodes to a single byte.
//!
//! # This decodes hostile input
//!
//! **Charter rule 14.** A client decodes this from a server it does not trust.
//! Every bound is checked before anything is allocated: run lengths are summed
//! against [`crate::BLOCKS_PER_CHUNK`] as they are read, so a header claiming
//! four billion blocks is refused at the third byte rather than after a huge
//! allocation. The decoder allocates exactly one fixed-size layer, once, and
//! never grows it.

use crate::BLOCKS_PER_CHUNK;

use super::{Fluid, FluidLayer};

/// A chunk with no fluid in it. No payload follows.
const TAG_EMPTY: u8 = 0;

/// Runs of `(count: u16, value: u8)`, little-endian.
const TAG_RUNS: u8 = 1;

/// Why a fluid payload would not decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FluidDecodeError {
    /// The payload was empty, so there is not even a tag.
    #[error("a fluid payload needs at least a tag byte")]
    Empty,

    /// The first byte named an encoding this build does not have.
    #[error("unknown fluid encoding {tag}")]
    UnknownTag {
        /// What the payload claimed.
        tag: u8,
    },

    /// A run header or value was cut short.
    #[error("a fluid payload ended in the middle of a run")]
    Truncated,

    /// A run of zero blocks, which cannot be encoded and would let a payload
    /// carry unbounded runs without ever covering the chunk.
    #[error("a fluid run covers no blocks")]
    EmptyRun,

    /// The runs cover more or fewer blocks than a chunk has.
    #[error("fluid runs cover {covered} blocks; a chunk has {expected}")]
    WrongLength {
        /// What the payload's runs added up to.
        covered: usize,
        /// What a chunk actually holds.
        expected: usize,
    },

    /// A byte that is not a value this build can represent.
    ///
    /// A level of zero paired with a fluid id, or an id paired with no level,
    /// are both writable as bytes and mean nothing — [`Fluid::EMPTY`] is the
    /// only encoding of "nothing". Refusing them keeps one value per state,
    /// which is what makes a hash of the layer mean something.
    #[error("fluid byte {value} is not a state a block can be in")]
    NotAState {
        /// The byte that was rejected.
        value: u8,
    },
}

/// Packs a layer.
///
/// An empty layer is one byte. Anything else is runs, which for a puddle in the
/// corner of a chunk is a handful.
#[must_use]
pub fn encode(layer: &FluidLayer) -> Vec<u8> {
    if layer.is_empty() {
        return vec![TAG_EMPTY];
    }

    let mut out = vec![TAG_RUNS];
    let mut blocks = layer.blocks();
    let mut current = blocks.next().unwrap_or(Fluid::EMPTY);
    let mut run: u16 = 1;
    for value in blocks {
        if value == current && run < u16::MAX {
            run += 1;
            continue;
        }
        out.extend_from_slice(&run.to_le_bytes());
        out.push(current.0);
        current = value;
        run = 1;
    }
    out.extend_from_slice(&run.to_le_bytes());
    out.push(current.0);
    out
}

/// Unpacks a layer.
///
/// # Errors
///
/// [`FluidDecodeError`] for anything that is not exactly one chunk's worth of
/// states this build can represent.
pub fn decode(bytes: &[u8]) -> Result<FluidLayer, FluidDecodeError> {
    let (&tag, rest) = bytes.split_first().ok_or(FluidDecodeError::Empty)?;
    match tag {
        TAG_EMPTY => {
            if rest.is_empty() {
                Ok(FluidLayer::empty())
            } else {
                Err(FluidDecodeError::WrongLength {
                    covered: BLOCKS_PER_CHUNK + rest.len(),
                    expected: BLOCKS_PER_CHUNK,
                })
            }
        }
        TAG_RUNS => {
            let mut values = Vec::with_capacity(BLOCKS_PER_CHUNK);
            let mut offset = 0;
            while offset < rest.len() {
                let count = rest
                    .get(offset..offset + 2)
                    .map(|pair| u16::from_le_bytes([pair[0], pair[1]]) as usize)
                    .ok_or(FluidDecodeError::Truncated)?;
                let value = *rest.get(offset + 2).ok_or(FluidDecodeError::Truncated)?;
                offset += 3;

                if count == 0 {
                    return Err(FluidDecodeError::EmptyRun);
                }
                let fluid = state(value)?;
                // **Checked before the push, not after.** Summing first is what
                // stops a payload of many long runs from growing the vector past
                // a chunk before anyone notices.
                if values.len() + count > BLOCKS_PER_CHUNK {
                    return Err(FluidDecodeError::WrongLength {
                        covered: values.len() + count,
                        expected: BLOCKS_PER_CHUNK,
                    });
                }
                values.extend(std::iter::repeat_n(fluid, count));
            }
            if values.len() != BLOCKS_PER_CHUNK {
                return Err(FluidDecodeError::WrongLength {
                    covered: values.len(),
                    expected: BLOCKS_PER_CHUNK,
                });
            }
            Ok(FluidLayer::from_blocks(values))
        }
        tag => Err(FluidDecodeError::UnknownTag { tag }),
    }
}

/// Reads one byte as a block state, refusing the ones that mean nothing.
fn state(value: u8) -> Result<Fluid, FluidDecodeError> {
    let fluid = Fluid(value);
    if fluid == Fluid::EMPTY {
        return Ok(Fluid::EMPTY);
    }
    // Exactly one encoding of nothing, and a source is always at full level.
    if fluid.fluid().is_none() || fluid.level() == 0 {
        return Err(FluidDecodeError::NotAState { value });
    }
    if fluid.is_source() && fluid.level() != super::MAX_LEVEL {
        return Err(FluidDecodeError::NotAState { value });
    }
    Ok(fluid)
}

#[cfg(test)]
mod tests {
    use super::super::{FluidId, MAX_LEVEL};
    use super::*;
    use crate::coords::LocalBlock;

    fn local(x: u32, y: u32, z: u32) -> LocalBlock {
        LocalBlock::new(x, y, z)
    }

    #[test]
    fn an_empty_chunk_is_one_byte() {
        // The common case in any world, so it is worth being exact about.
        let encoded = encode(&FluidLayer::empty());
        assert_eq!(encoded, vec![TAG_EMPTY]);
        assert_eq!(decode(&encoded), Ok(FluidLayer::empty()));
    }

    #[test]
    fn a_puddle_round_trips() {
        let milk = FluidId(1);
        let mut layer = FluidLayer::empty();
        layer.set(local(0, 0, 0), Fluid::source(milk));
        layer.set(local(1, 0, 0), Fluid::flowing(milk, 6));
        layer.set(local(2, 0, 0), Fluid::flowing(milk, 5));
        layer.set(local(15, 15, 15), Fluid::flowing(milk, 1));

        let decoded = decode(&encode(&layer)).expect("round trip");
        assert_eq!(decoded, layer);
    }

    #[test]
    fn a_puddle_costs_a_handful_of_bytes() {
        // The claim the module docs make. Three runs plus a tag.
        let milk = FluidId(1);
        let mut layer = FluidLayer::empty();
        layer.set(local(0, 0, 0), Fluid::source(milk));
        assert!(
            encode(&layer).len() <= 1 + 3 * 3,
            "one block of milk encoded to {} bytes",
            encode(&layer).len()
        );
    }

    #[test]
    fn a_full_chunk_round_trips() {
        let milk = FluidId(1);
        let layer =
            FluidLayer::from_blocks(std::iter::repeat_n(Fluid::source(milk), BLOCKS_PER_CHUNK));
        let encoded = encode(&layer);
        assert_eq!(encoded.len(), 1 + 3, "a uniform chunk is one run");
        assert_eq!(decode(&encoded), Ok(layer));
    }

    #[test]
    fn hostile_payloads_are_refused_rather_than_allocated_for() {
        assert_eq!(decode(&[]), Err(FluidDecodeError::Empty));
        assert_eq!(decode(&[9]), Err(FluidDecodeError::UnknownTag { tag: 9 }));
        assert_eq!(decode(&[TAG_RUNS, 1]), Err(FluidDecodeError::Truncated));
        assert_eq!(decode(&[TAG_RUNS, 1, 0]), Err(FluidDecodeError::Truncated));
        assert_eq!(
            decode(&[TAG_RUNS, 0, 0, 0]),
            Err(FluidDecodeError::EmptyRun)
        );

        // A run claiming the whole u16 range, twice over: refused on the second
        // header rather than after the vector has grown past a chunk.
        let mut runaway = vec![TAG_RUNS];
        for _ in 0..2 {
            runaway.extend_from_slice(&u16::MAX.to_le_bytes());
            runaway.push(0);
        }
        assert!(matches!(
            decode(&runaway),
            Err(FluidDecodeError::WrongLength { .. })
        ));
    }

    #[test]
    fn bytes_that_are_not_states_are_refused() {
        // **One encoding per state, or a layer hash means nothing.** A fluid id
        // with no level, and a source that is not full, are both writable and
        // neither is reachable.
        let id_without_level = 1 << 4;
        assert_eq!(
            state(id_without_level),
            Err(FluidDecodeError::NotAState {
                value: id_without_level
            })
        );
        let level_without_id = 3;
        assert_eq!(
            state(level_without_id),
            Err(FluidDecodeError::NotAState {
                value: level_without_id
            })
        );
        let half_drained_source = (1 << 4) | 0b1000 | 3;
        assert_eq!(
            state(half_drained_source),
            Err(FluidDecodeError::NotAState {
                value: half_drained_source
            })
        );
        // And the reachable ones are accepted.
        assert!(state(Fluid::source(FluidId(1)).0).is_ok());
        assert!(state(Fluid::flowing(FluidId(1), MAX_LEVEL).0).is_ok());
        assert!(state(Fluid::EMPTY.0).is_ok());
    }

    #[test]
    fn a_short_payload_is_a_length_error_rather_than_a_short_layer() {
        let mut short = vec![TAG_RUNS];
        short.extend_from_slice(&1u16.to_le_bytes());
        short.push(0);
        assert!(matches!(
            decode(&short),
            Err(FluidDecodeError::WrongLength { covered: 1, .. })
        ));
    }
}
