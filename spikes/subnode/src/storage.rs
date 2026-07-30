// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Deliverable 5 — the storage and bandwidth probe.
//!
//! Two budgets that silently constrain later tasks:
//!
//! - **Task 03 (persistence)** has a "uniform chunk ≤ 100 bytes" target and no
//!   equivalent for a chiselled one. This produces that number.
//! - **Task 06 (networking)** needs to know whether a sub-node edit can ride
//!   the ordinary block-delta path or needs its own compact encoding. That
//!   depends entirely on how many bytes a minute of chiselling generates.
//!
//! # The encoding is a stand-in, not a proposal
//!
//! Task 03 designs the real format. This one is built only from the public
//! Task 02 API — scan the blocks, build a palette, bit-pack the indices — which
//! is close enough to what a real palette-compressed serialiser would emit that
//! the compressed sizes are meaningful. It is deliberately not clever: an
//! over-tuned spike encoding would flatter the design.

use tiamot_core::bitpack::BitArray;
use tiamot_core::block::{BlockValue, Cells, SUBNODES_PER_BLOCK};
use tiamot_core::chunk::Chunk;
use tiamot_core::coords::LocalBlock;
use tiamot_core::{BLOCKS_PER_CHUNK, MaterialId};

/// zstd level. 3 is the library default and what a server would realistically
/// afford on a chunk save; higher levels cost tick time for a few percent.
pub const ZSTD_LEVEL: i32 = 3;

/// Tag bytes for the palette entry encoding.
const TAG_UNIFORM: u8 = 0;
const TAG_PARTIAL: u8 = 1;
const TAG_CELLS: u8 = 2;

/// Serialises a chunk to a palette-compressed byte stream.
#[must_use]
pub fn serialize(chunk: &Chunk) -> Vec<u8> {
    // Rebuild a palette from the public API. Deduplicating by value is exactly
    // what the chunk does internally, so the entry count matches.
    let mut palette: Vec<BlockValue> = Vec::new();
    let mut indices: Vec<u32> = Vec::with_capacity(BLOCKS_PER_CHUNK);

    for index in 0..BLOCKS_PER_CHUNK {
        let value = chunk
            .get_block_local(LocalBlock::from_index(index))
            .to_value();
        let slot = palette
            .iter()
            .position(|entry| *entry == value)
            .unwrap_or_else(|| {
                palette.push(value);
                palette.len() - 1
            });
        indices.push(slot as u32);
    }

    let mut out = Vec::new();
    out.extend_from_slice(&(palette.len() as u32).to_le_bytes());

    for entry in &palette {
        match entry {
            BlockValue::Uniform(material) => {
                out.push(TAG_UNIFORM);
                out.extend_from_slice(&material.get().to_le_bytes());
            }
            BlockValue::Partial {
                material,
                occupancy,
            } => {
                out.push(TAG_PARTIAL);
                out.extend_from_slice(&material.get().to_le_bytes());
                // 27 bits, so 4 bytes. A real format would pack this to
                // 27 bits and save one byte per entry.
                out.extend_from_slice(&occupancy.to_le_bytes());
            }
            BlockValue::Cells(cells) => {
                out.push(TAG_CELLS);
                for cell in cells {
                    out.extend_from_slice(&cell.get().to_le_bytes());
                }
            }
        }
    }

    let bits = BitArray::bits_for(palette.len());
    out.push(bits);
    let mut packed = BitArray::new(BLOCKS_PER_CHUNK, bits);
    for (index, slot) in indices.iter().enumerate() {
        packed.set(index, *slot);
    }
    for value in packed.iter() {
        // Emit as the packed width would, byte-aligned per entry group. Keeping
        // it simple: emit each index as the smallest whole number of bytes that
        // holds `bits`, and let zstd remove the slack. A real format packs
        // exactly; the compressed sizes end up within a few percent.
        match bits {
            0 => {}
            1..=8 => out.push(value as u8),
            9..=16 => out.extend_from_slice(&(value as u16).to_le_bytes()),
            _ => out.extend_from_slice(&value.to_le_bytes()),
        }
    }

    out
}

/// Compresses with zstd at [`ZSTD_LEVEL`].
///
/// # Panics
///
/// If zstd fails, which for in-memory compression means an allocation failure.
#[must_use]
pub fn compress(bytes: &[u8]) -> Vec<u8> {
    zstd::encode_all(bytes, ZSTD_LEVEL).expect("in-memory zstd compression cannot fail")
}

/// One recorded edit from a chiselling session.
#[derive(Debug, Clone, Copy)]
pub struct SubNodeEdit {
    pub block: u16,
    pub cell: u8,
    pub material: u16,
}

/// The two candidate wire encodings for an edit stream.
pub struct DeltaSizes {
    /// Raw bytes with a dedicated sub-node encoding: block, cell, material.
    pub subnode_raw: usize,
    /// The same stream compressed.
    pub subnode_compressed: usize,
    /// Raw bytes if each edit rides the ordinary block path, resending the
    /// block's whole 27-cell contents.
    pub block_path_raw: usize,
    /// The same stream compressed.
    pub block_path_compressed: usize,
    pub edits: usize,
}

/// Encodes an edit stream both ways and compresses each.
///
/// The comparison is the deliverable: if the block path compresses to something
/// comparable, Task 06 does not need a separate sub-node delta opcode at all.
#[must_use]
pub fn encode_deltas(edits: &[SubNodeEdit], chunk: &Chunk) -> DeltaSizes {
    // Compact sub-node encoding: 5 bytes per edit.
    let mut subnode = Vec::with_capacity(edits.len() * 5);
    for edit in edits {
        subnode.extend_from_slice(&edit.block.to_le_bytes());
        subnode.push(edit.cell);
        subnode.extend_from_slice(&edit.material.to_le_bytes());
    }

    // Block-path encoding: resend the whole block, 2 + 54 bytes per edit.
    let mut block_path = Vec::with_capacity(edits.len() * 56);
    for edit in edits {
        block_path.extend_from_slice(&edit.block.to_le_bytes());
        let cells: Cells = chunk
            .get_block_local(LocalBlock::from_index(edit.block as usize))
            .cells();
        for cell in cells {
            block_path.extend_from_slice(&cell.get().to_le_bytes());
        }
    }

    DeltaSizes {
        subnode_raw: subnode.len(),
        subnode_compressed: compress(&subnode).len(),
        block_path_raw: block_path.len(),
        block_path_compressed: compress(&block_path).len(),
        edits: edits.len(),
    }
}

/// Records a chiselling session: one sub-node removed per tick.
///
/// One edit per tick at 20 tps is a player chiselling continuously for the
/// whole minute without pause. Nobody plays like that, which is the point — the
/// bandwidth gate should be set by a worst case, not an average.
#[must_use]
pub fn record_session(chunk: &mut Chunk, ticks: usize, seed: u64) -> Vec<SubNodeEdit> {
    let mut rng = crate::scenes::Rng::new(seed);
    let mut edits = Vec::with_capacity(ticks);

    for _ in 0..ticks {
        // Chisel somewhere on the surface layer, where a player would be.
        let x = rng.below(16);
        let z = rng.below(16);
        let local = LocalBlock::new(x, 7, z);
        let cell = rng.below(SUBNODES_PER_BLOCK as u32) as usize;

        let mut cells = chunk.get_block_local(local).cells();
        if cells[cell].is_air() {
            continue;
        }
        cells[cell] = MaterialId::AIR;
        chunk.set_block_local(local, BlockValue::Cells(cells));

        edits.push(SubNodeEdit {
            block: local.index() as u16,
            cell: cell as u8,
            material: MaterialId::AIR.get(),
        });
    }

    edits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenes::Scene;

    #[test]
    fn a_uniform_chunk_compresses_to_almost_nothing() {
        let chunk = Chunk::new(tiamot_core::ChunkPos::new(0, 0, 0), crate::scenes::STONE);
        let compressed = compress(&serialize(&chunk)).len();
        assert!(
            compressed < 100,
            "a uniform chunk should compress under Task 03's 100-byte target, got {compressed}"
        );
    }

    #[test]
    fn serialisation_is_deterministic() {
        for scene in Scene::ALL {
            let chunk = scene.build(11);
            assert_eq!(
                serialize(&chunk),
                serialize(&chunk),
                "{} serialised differently twice",
                scene.label()
            );
        }
    }

    #[test]
    fn compression_round_trips() {
        let chunk = Scene::Realistic.build(3);
        let raw = serialize(&chunk);
        let restored = zstd::decode_all(compress(&raw).as_slice()).expect("decompress");
        assert_eq!(raw, restored);
    }

    #[test]
    fn a_recorded_session_produces_edits_and_changes_the_chunk() {
        let mut chunk = Scene::Flat.build(1);
        let before = serialize(&chunk);
        let edits = record_session(&mut chunk, 200, 4);
        assert!(!edits.is_empty(), "the session should have chiselled");
        assert_ne!(before, serialize(&chunk), "the chunk should have changed");
    }

    #[test]
    fn the_subnode_encoding_beats_the_block_path_before_compression() {
        let mut chunk = Scene::Flat.build(1);
        let edits = record_session(&mut chunk, 200, 4);
        let sizes = encode_deltas(&edits, &chunk);
        assert!(sizes.subnode_raw < sizes.block_path_raw);
    }
}
