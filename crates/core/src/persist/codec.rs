// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Chunk blob encoding: `postcard`, then `zstd`, behind a two-byte header.
//!
//! # Blob layout
//!
//! ```text
//! byte 0   format version   (see CHUNK_FORMAT_VERSION)
//! byte 1   dictionary id    (0 = no dictionary)
//! byte 2.. `zstd`-compressed `postcard` payload
//! ```
//!
//! The version byte comes first and outside the compression so a blob can be
//! identified without decompressing it — which matters because deciding whether
//! to migrate has to happen before deciding how to decode.
//!
//! # `postcard` variants are position-encoded
//!
//! **This is the sharpest edge in the format.** `postcard` writes an enum variant
//! as its ordinal. Inserting a variant anywhere but the end of a `#[derive]`d
//! enum shifts every later ordinal, and every existing world file silently
//! decodes into different variants — no error, no checksum failure, just wrong
//! blocks.
//!
//! The rule, for every type reachable from [`ChunkPayload`]:
//!
//! 1. **New enum variants go at the end. Always.**
//! 2. **Never remove or reorder a variant.** Deprecate in place.
//! 3. Any change to a serialised type bumps [`CHUNK_FORMAT_VERSION`] and adds a
//!    migration step in [`super::migrate`].
//!
//! [`crate::block::BlockContent`] carries this warning at its definition too.
//!
//! # Materials are translated on the way in and out
//!
//! Blobs store **world** numeric ids, never runtime ones (charter rule 8). See
//! [`super::idmap`] for why, and what happens when a mod goes missing.

use serde::{Deserialize, Serialize};

use crate::block::{BlockContent, Cells};
use crate::chunk::{Chunk, ChunkParts};
use crate::coords::ChunkPos;
use crate::material::MaterialId;
use crate::persist::idmap::{IdMapError, MaterialMap};

/// Current chunk blob format version.
///
/// Bump this for any change to [`ChunkPayload`] or anything it contains, and
/// add the matching step to [`super::migrate::migrate_chunk`].
pub const CHUNK_FORMAT_VERSION: u8 = 1;

/// Dictionary id meaning "compressed without a dictionary".
pub const NO_DICTIONARY: u8 = 0;

/// `zstd` level for chunk blobs.
///
/// 3 is the library default. A server saves chunks on the tick thread's budget,
/// and levels above about 6 cost several times the time for a few percent of
/// size on data this small.
///
/// # Measured stored sizes
///
/// From `benches/persist.rs`, which reports them on every run:
///
/// | Chunk | Stored | Task 02b spike |
/// |---|---|---|
/// | Uniform | **20 B** | 157 B |
/// | Flat terrain, 2 materials | **55 B** | — |
/// | Every surface block chiselled | **937 B** | 1,797 B |
///
/// The Task 02b column is the spike's deliberately naive encoding, recorded in
/// `docs/subnode-verdict.md`. The real format is roughly twice as compact
/// because it packs block indices to the palette width and stores mixed cells
/// once per distinct array rather than once per block.
///
/// Task 03's target was a uniform chunk at 100 bytes or fewer. The measured
/// 20 bytes clears it by 5x, and the chiselled figure sets the budget Task 06's
/// chunk streaming should be designed against: **under 1 KiB for a heavily
/// built-in chunk.**
pub const ZSTD_LEVEL: i32 = 3;

/// Bytes of header before the compressed payload.
const HEADER_LEN: usize = 2;

/// The serialised form of a chunk's contents.
///
/// Mirrors [`ChunkParts`] but with **world** material ids substituted for
/// runtime ones. Kept as a separate type rather than serialising `ChunkParts`
/// directly so that the translated and untranslated forms cannot be confused —
/// they have identical shapes and opposite meanings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkPayload {
    /// Palette entries with world material ids, and their refcounts.
    pub palette: Vec<(BlockContent, u32)>,
    /// Bits per packed block index.
    pub bits_per_index: u8,
    /// The packed index words.
    pub index_words: Vec<u64>,
    /// Mixed-block cell arrays, with world material ids.
    pub mixed_cells: Vec<Cells>,
    /// Reclaimed mixed slots.
    pub mixed_free: Vec<u16>,
}

/// Encoding or decoding a chunk blob failed.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    /// The blob is shorter than its header.
    #[error("chunk blob is {len} bytes, shorter than the {HEADER_LEN}-byte header")]
    Truncated {
        /// Length received.
        len: usize,
    },

    /// The blob claims a format version this build does not know.
    #[error(
        "chunk blob claims format version {found}, but this build understands at most \
         {CHUNK_FORMAT_VERSION} — the world was written by a newer engine"
    )]
    FutureVersion {
        /// Version claimed.
        found: u8,
    },

    /// The blob names a dictionary the world does not have.
    #[error("chunk blob references dictionary {id}, which is not stored in this world")]
    MissingDictionary {
        /// Dictionary id claimed.
        id: u8,
    },

    /// Decompression failed.
    #[error("could not decompress chunk blob")]
    Decompress(#[source] std::io::Error),

    /// Compression failed.
    #[error("could not compress chunk blob")]
    Compress(#[source] std::io::Error),

    /// The payload is not valid `postcard`, or does not match the schema.
    #[error("chunk blob payload is malformed")]
    Payload(#[source] postcard::Error),

    /// Material translation failed.
    #[error("could not translate material ids")]
    Materials(#[source] IdMapError),

    /// The decoded payload does not describe a valid chunk.
    #[error("chunk blob decoded but failed validation")]
    Invalid(#[source] crate::chunk::CorruptChunk),

    /// Migration from an older format failed.
    #[error("could not migrate chunk blob from format version {from}")]
    Migration {
        /// Version migrated from.
        from: u8,
        /// Why it failed.
        #[source]
        source: super::migrate::MigrationError,
    },
}

impl From<IdMapError> for CodecError {
    fn from(source: IdMapError) -> Self {
        Self::Materials(source)
    }
}

/// A `zstd` dictionary stored in the world, with the id blobs reference it by.
#[derive(Debug, Clone)]
pub struct Dictionary {
    /// Id written into the blob header. Never [`NO_DICTIONARY`].
    pub id: u8,
    /// Raw dictionary bytes.
    pub bytes: Vec<u8>,
}

/// Encodes a chunk.
///
/// Pass `dictionary` to compress against a shared `zstd` dictionary. Nothing
/// trains one yet — the code path exists now so that adding a trained
/// dictionary later is a data change rather than a format change, and so that
/// old dictionary-less blobs keep decoding forever.
///
/// # Errors
///
/// [`CodecError`] if material translation, serialisation, or compression fails.
pub fn encode_chunk(
    chunk: &Chunk,
    materials: &MaterialMap,
    dictionary: Option<&Dictionary>,
) -> Result<Vec<u8>, CodecError> {
    let payload = to_payload(&chunk.to_parts(), materials)?;
    let serialised = postcard::to_allocvec(&payload).map_err(CodecError::Payload)?;

    let compressed = match dictionary {
        None => zstd::bulk::compress(&serialised, ZSTD_LEVEL).map_err(CodecError::Compress)?,
        Some(dict) => zstd::bulk::Compressor::with_dictionary(ZSTD_LEVEL, &dict.bytes)
            .and_then(|mut compressor| compressor.compress(&serialised))
            .map_err(CodecError::Compress)?,
    };

    let mut blob = Vec::with_capacity(HEADER_LEN + compressed.len());
    blob.push(CHUNK_FORMAT_VERSION);
    blob.push(dictionary.map_or(NO_DICTIONARY, |dict| dict.id));
    blob.extend_from_slice(&compressed);
    Ok(blob)
}

/// The format version a blob claims, without decoding it.
///
/// # Errors
///
/// [`CodecError::Truncated`] if the blob has no header.
pub fn blob_version(blob: &[u8]) -> Result<u8, CodecError> {
    blob.first()
        .copied()
        .ok_or(CodecError::Truncated { len: blob.len() })
}

/// Decodes a chunk, migrating it forward if it was written by an older format.
///
/// `dictionaries` is consulted only when the blob names one.
///
/// # Errors
///
/// [`CodecError`] for a truncated, future-versioned, corrupt, or
/// unmigratable blob. **Every one of these is reachable from a damaged save
/// file, so none of them panics.**
pub fn decode_chunk(
    pos: ChunkPos,
    blob: &[u8],
    materials: &MaterialMap,
    dictionaries: &[Dictionary],
) -> Result<Chunk, CodecError> {
    if blob.len() < HEADER_LEN {
        return Err(CodecError::Truncated { len: blob.len() });
    }

    let version = blob[0];
    let dictionary_id = blob[1];
    let body = &blob[HEADER_LEN..];

    if version > CHUNK_FORMAT_VERSION {
        return Err(CodecError::FutureVersion { found: version });
    }

    let serialised = decompress(body, dictionary_id, dictionaries)?;

    // Migrate at the postcard level, before material translation: an old
    // payload may not even have the same fields.
    let payload = if version == CHUNK_FORMAT_VERSION {
        postcard::from_bytes::<ChunkPayload>(&serialised).map_err(CodecError::Payload)?
    } else {
        super::migrate::migrate_chunk(version, &serialised).map_err(|source| {
            CodecError::Migration {
                from: version,
                source,
            }
        })?
    };

    let parts = from_payload(payload, materials)?;
    Chunk::from_parts(pos, parts).map_err(CodecError::Invalid)
}

fn decompress(
    body: &[u8],
    dictionary_id: u8,
    dictionaries: &[Dictionary],
) -> Result<Vec<u8>, CodecError> {
    // A chunk decompresses to a bounded size: the palette can hold at most one
    // entry per block, each at most a 27-cell array, plus the index words. This
    // cap is what stops a hand-crafted blob from being a decompression bomb.
    const MAX_DECOMPRESSED: usize = 4 * 1024 * 1024;

    if dictionary_id == NO_DICTIONARY {
        return zstd::bulk::decompress(body, MAX_DECOMPRESSED).map_err(CodecError::Decompress);
    }

    let dictionary = dictionaries
        .iter()
        .find(|candidate| candidate.id == dictionary_id)
        .ok_or(CodecError::MissingDictionary { id: dictionary_id })?;

    zstd::bulk::Decompressor::with_dictionary(&dictionary.bytes)
        .and_then(|mut decompressor| decompressor.decompress(body, MAX_DECOMPRESSED))
        .map_err(CodecError::Decompress)
}

/// Runtime ids → world ids.
fn to_payload(parts: &ChunkParts, materials: &MaterialMap) -> Result<ChunkPayload, CodecError> {
    let mut palette = Vec::with_capacity(parts.palette.len());
    for (content, refs) in &parts.palette {
        palette.push((
            translate_content(*content, |id| materials.to_world(id))?,
            *refs,
        ));
    }

    let mut mixed_cells = Vec::with_capacity(parts.mixed_cells.len());
    for cells in &parts.mixed_cells {
        mixed_cells.push(translate_cells(cells, |id| materials.to_world(id))?);
    }

    Ok(ChunkPayload {
        palette,
        bits_per_index: parts.bits_per_index,
        index_words: parts.index_words.clone(),
        mixed_cells,
        mixed_free: parts.mixed_free.clone(),
    })
}

/// World ids → runtime ids.
fn from_payload(payload: ChunkPayload, materials: &MaterialMap) -> Result<ChunkParts, CodecError> {
    let mut palette = Vec::with_capacity(payload.palette.len());
    for (content, refs) in payload.palette {
        palette.push((
            translate_content(content, |id| materials.to_runtime(id.get()))?,
            refs,
        ));
    }

    let mut mixed_cells = Vec::with_capacity(payload.mixed_cells.len());
    for cells in &payload.mixed_cells {
        mixed_cells.push(translate_cells(cells, |id| materials.to_runtime(id.get()))?);
    }

    Ok(ChunkParts {
        palette,
        bits_per_index: payload.bits_per_index,
        index_words: payload.index_words,
        mixed_cells,
        mixed_free: payload.mixed_free,
    })
}

/// Applies a material translation to a palette entry.
///
/// Generic over the direction so both ways use one implementation — the two
/// were separate functions once and drifted, which is exactly the kind of bug
/// that only shows up after a mod is removed.
fn translate_content<F, E>(content: BlockContent, translate: F) -> Result<BlockContent, CodecError>
where
    F: Fn(MaterialId) -> Result<E, IdMapError>,
    E: Into<MaterialId>,
{
    Ok(match content {
        BlockContent::Uniform(material) => BlockContent::Uniform(translate(material)?.into()),
        BlockContent::Partial {
            material,
            occupancy,
        } => BlockContent::Partial {
            material: translate(material)?.into(),
            occupancy,
        },
        BlockContent::Mixed(slot) => BlockContent::Mixed(slot),
    })
}

fn translate_cells<F, E>(cells: &Cells, translate: F) -> Result<Cells, CodecError>
where
    F: Fn(MaterialId) -> Result<E, IdMapError>,
    E: Into<MaterialId>,
{
    let mut out = crate::block::EMPTY_CELLS;
    for (slot, cell) in cells.iter().enumerate() {
        out[slot] = translate(*cell)?.into();
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::material::MaterialRegistry;
    use crate::persist::idmap::IdTable;
    use crate::persist::schema;
    use crate::{BlockValue, Registry};
    use rusqlite::Connection;

    fn session(names: &[&str]) -> (Connection, MaterialMap, Registry) {
        let conn = Connection::open_in_memory().expect("open");
        schema::create(&conn).expect("schema");
        let mut registry = Registry::new();
        for name in names {
            registry.register(name).expect("register");
        }
        let mut table = IdTable::load(&conn).expect("load");
        let map = table.reconcile(&conn, &mut registry).expect("reconcile");
        (conn, map, registry)
    }

    fn origin() -> ChunkPos {
        ChunkPos::new(0, 0, 0)
    }

    #[test]
    fn a_uniform_chunk_round_trips() {
        let (_conn, map, registry) = session(&["core:white"]);
        let white = registry.id_of("core:white").expect("registered");
        let chunk = Chunk::new(origin(), white);

        let blob = encode_chunk(&chunk, &map, None).expect("encode");
        let restored = decode_chunk(origin(), &blob, &map, &[]).expect("decode");
        assert_eq!(chunk, restored);
    }

    #[test]
    fn a_uniform_white_chunk_fits_in_a_hundred_bytes() {
        // Task 03 acceptance criterion. Asserted loosely: the exact size is a
        // property of zstd's framing, not of the design, and pinning it would
        // make a zstd upgrade look like a regression.
        let (_conn, map, registry) = session(&["core:white"]);
        let white = registry.id_of("core:white").expect("registered");
        let blob = encode_chunk(&Chunk::new(origin(), white), &map, None).expect("encode");
        assert!(
            blob.len() <= 100,
            "uniform white chunk stored as {} bytes, over the 100-byte target",
            blob.len()
        );
    }

    #[test]
    fn a_mixed_chunk_round_trips() {
        let (_conn, map, registry) = session(&["core:stone", "core:dirt"]);
        let stone = registry.id_of("core:stone").expect("registered");
        let dirt = registry.id_of("core:dirt").expect("registered");

        let mut chunk = Chunk::air(origin());
        let mut cells = crate::block::EMPTY_CELLS;
        cells[0] = stone;
        cells[5] = dirt;
        chunk.set_block_local(
            crate::coords::LocalBlock::new(3, 4, 5),
            BlockValue::Cells(cells),
        );
        chunk.set_block_local(
            crate::coords::LocalBlock::new(1, 1, 1),
            BlockValue::Partial {
                material: stone,
                occupancy: 0b1011,
            },
        );

        let blob = encode_chunk(&chunk, &map, None).expect("encode");
        let restored = decode_chunk(origin(), &blob, &map, &[]).expect("decode");
        assert_eq!(chunk, restored, "exact internal state must survive");
    }

    #[test]
    fn a_truncated_blob_is_an_error_not_a_panic() {
        let (_conn, map, _registry) = session(&[]);
        for len in 0..HEADER_LEN {
            assert!(matches!(
                decode_chunk(origin(), &vec![0u8; len], &map, &[]),
                Err(CodecError::Truncated { .. })
            ));
        }
    }

    #[test]
    fn a_future_version_is_refused_clearly() {
        let (_conn, map, _registry) = session(&[]);
        let blob = vec![CHUNK_FORMAT_VERSION + 1, NO_DICTIONARY, 0, 0];
        let err = decode_chunk(origin(), &blob, &map, &[]).expect_err("should refuse");
        assert!(matches!(err, CodecError::FutureVersion { .. }));
        assert!(err.to_string().contains("newer engine"));
    }

    #[test]
    fn garbage_after_the_header_is_an_error_not_a_panic() {
        let (_conn, map, _registry) = session(&[]);
        let blob = vec![CHUNK_FORMAT_VERSION, NO_DICTIONARY, 0xDE, 0xAD, 0xBE, 0xEF];
        assert!(decode_chunk(origin(), &blob, &map, &[]).is_err());
    }

    #[test]
    fn a_missing_dictionary_is_named_in_the_error() {
        let (_conn, map, _registry) = session(&[]);
        let blob = vec![CHUNK_FORMAT_VERSION, 7, 0, 0];
        assert!(matches!(
            decode_chunk(origin(), &blob, &map, &[]),
            Err(CodecError::MissingDictionary { id: 7 })
        ));
    }

    #[test]
    fn a_dictionary_round_trips() {
        let (_conn, map, registry) = session(&["core:stone"]);
        let stone = registry.id_of("core:stone").expect("registered");
        let chunk = Chunk::new(origin(), stone);

        // Not a trained dictionary — just proving the code path carries the id
        // and decodes against the right bytes.
        let dictionary = Dictionary {
            id: 1,
            bytes: vec![0u8; 1024],
        };
        let blob = encode_chunk(&chunk, &map, Some(&dictionary)).expect("encode");
        assert_eq!(blob[1], 1, "the dictionary id must be in the header");

        let restored =
            decode_chunk(origin(), &blob, &map, std::slice::from_ref(&dictionary)).expect("decode");
        assert_eq!(chunk, restored);
    }

    #[test]
    fn blob_version_reads_without_decoding() {
        let (_conn, map, _registry) = session(&[]);
        let blob = encode_chunk(&Chunk::air(origin()), &map, None).expect("encode");
        assert_eq!(blob_version(&blob).expect("version"), CHUNK_FORMAT_VERSION);
    }
}
