// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Serving content to clients: chunked, compressed, and rate-limited.
//!
//! # Every request is adversarial
//!
//! Charter rule 14 is usually read from the client's side — a client decodes
//! assets from servers it does not trust. The mirror applies here: a *client*
//! can ask for anything, repeatedly, from an unauthenticated-adjacent position,
//! and the server has to stay up.
//!
//! Three limits, each for a different attack:
//!
//! - **Unknown hashes are refused silently.** Answering "no such content"
//!   differently from "here it is" turns the server into an oracle for probing
//!   what a private mod pack contains.
//! - **A per-client byte quota** bounds what one connection can pull. Without
//!   it, a client asking for the whole index on a loop is an amplifier: cheap
//!   to request, expensive to serve.
//! - **A per-tick send budget** bounds how much work a client can cause *at
//!   once*, so a big transfer is slow rather than a stall for everyone else.
//!
//! # Compression is per slice, not per file
//!
//! A client can decode a slice as it arrives rather than buffering the whole
//! file first. That matters for the memory cap on the client side — charter
//! rule 14 requires pre-decode bounds, and a bound you can only check after
//! buffering 16 MB is not much of a bound.

use std::collections::BTreeSet;

use tiamot_core::content::ContentIndex;
use tiamot_core::proto::{ContentHash, MAX_CONTENT_CHUNK_BYTES, ServerMessage};

/// Uncompressed bytes per slice.
///
/// Sized so a compressed slice stays comfortably under
/// [`MAX_CONTENT_CHUNK_BYTES`] even for incompressible data, where zstd adds a
/// small framing overhead rather than shrinking anything. Picking the limit
/// itself would put every incompressible file one byte over.
pub const SLICE_BYTES: usize = 192 * 1024;

/// zstd level for content slices.
///
/// 3 is zstd's default and the knee of its curve: most of the ratio for a small
/// fraction of the time. Content is compressed on the fly while players wait,
/// so spending 10× the CPU for a few percent would be the wrong trade.
pub const COMPRESSION_LEVEL: i32 = 3;

/// Total bytes one connection may pull per session.
///
/// Generous next to any real mod pack, and a hard stop on a client looping
/// requests to burn server CPU. A client that legitimately needs more than this
/// is downloading more than [`tiamot_core::content::MAX_MOD_BYTES`] allows a
/// server to hold.
pub const QUOTA_BYTES: u64 = 512 * 1024 * 1024;

/// Slices sent to one client per pass.
///
/// Bounds how much work a single request can cause at once. A large transfer
/// becomes slow rather than a stall for everyone else on the server.
pub const SLICES_PER_PASS: usize = 2;

/// One client's outstanding content transfers.
#[derive(Debug, Default)]
pub struct Transfers {
    /// Hashes queued to send, in request order.
    queue: Vec<ContentHash>,
    /// Hashes already queued or sent, so a repeated request is a no-op.
    seen: BTreeSet<ContentHash>,
    /// Byte offset into the item currently being sent.
    offset: usize,
    /// Bytes sent to this client so far.
    sent_bytes: u64,
}

impl Transfers {
    /// A fresh transfer state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many items are still queued.
    #[must_use]
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// Bytes sent to this client.
    #[must_use]
    pub const fn sent_bytes(&self) -> u64 {
        self.sent_bytes
    }

    /// Whether this client has exhausted its quota.
    #[must_use]
    pub const fn over_quota(&self) -> bool {
        self.sent_bytes >= QUOTA_BYTES
    }

    /// Queues the hashes a client asked for that the server actually has.
    ///
    /// Returns how many were accepted. Unknown hashes are **dropped without
    /// comment**: distinguishing "no such content" from "here it is" turns the
    /// server into an oracle for probing what a private mod pack contains.
    ///
    /// A hash already queued or sent is ignored, so a client repeating its
    /// request cannot make the server send the same file twice.
    pub fn request(&mut self, hashes: &[ContentHash], index: &ContentIndex) -> usize {
        let mut accepted = 0;
        for hash in hashes {
            if self.seen.contains(hash) || index.get(hash).is_none() {
                continue;
            }
            self.seen.insert(*hash);
            self.queue.push(*hash);
            accepted += 1;
        }
        accepted
    }

    /// Produces up to [`SLICES_PER_PASS`] messages of content to send.
    ///
    /// Returns an empty vector when there is nothing queued or the quota is
    /// spent.
    pub fn next_slices(&mut self, index: &ContentIndex) -> Vec<ServerMessage> {
        let mut out = Vec::new();

        while out.len() < SLICES_PER_PASS {
            if self.over_quota() {
                // Stop sending rather than disconnecting. A client that hit the
                // quota has almost certainly finished anyway, and dropping a
                // connection over a byte count would punish a legitimately
                // large mod pack the same as an attack.
                break;
            }
            let Some(hash) = self.queue.first().copied() else {
                break;
            };
            let Some(item) = index.get(&hash) else {
                // Vanished between request and send. Cannot happen with a
                // frozen index, but dropping the entry is the right response if
                // the index ever becomes reloadable.
                self.queue.remove(0);
                self.offset = 0;
                continue;
            };

            let total_len = item.bytes.len();
            let end = (self.offset + SLICE_BYTES).min(total_len);
            let slice = &item.bytes[self.offset..end];

            let compressed = match zstd::encode_all(slice, COMPRESSION_LEVEL) {
                Ok(compressed) => compressed,
                Err(err) => {
                    // Compression failing is a bug rather than a condition, but
                    // dropping the item beats stalling the client forever on an
                    // item that will never encode.
                    tracing::error!("could not compress content slice: {err}");
                    self.queue.remove(0);
                    self.offset = 0;
                    continue;
                }
            };

            debug_assert!(
                compressed.len() <= MAX_CONTENT_CHUNK_BYTES,
                "a compressed slice must fit the protocol's chunk limit"
            );

            out.push(ServerMessage::ContentChunk {
                hash,
                offset: self.offset as u64,
                total_len: total_len as u64,
                data: compressed,
            });

            self.sent_bytes = self.sent_bytes.saturating_add((end - self.offset) as u64);
            self.offset = end;

            // A zero-length file sends exactly one empty slice, then finishes.
            // Without the `>=` an empty item would loop forever producing
            // nothing: `offset` and `total_len` are both 0, so `offset < total`
            // is never true and the item never leaves the queue.
            if self.offset >= total_len {
                self.queue.remove(0);
                self.offset = 0;
            }
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use tiamot_core::content::hash_bytes;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("tiamot-content-serve-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn write(dir: &Path, relative: &str, bytes: &[u8]) {
        std::fs::write(dir.join(relative), bytes).expect("write");
    }

    /// An index holding one file of `len` bytes of compressible data.
    fn index_with(name: &str, len: usize) -> (ContentIndex, ContentHash, Vec<u8>) {
        let dir = scratch(name);
        let bytes: Vec<u8> = (0..len)
            .map(|i| u8::try_from(i % 251).unwrap_or(0))
            .collect();
        write(&dir, "asset.png", &bytes);
        let mut index = ContentIndex::new();
        index.add_mod("x", &dir).expect("index");
        let hash = hash_bytes(&bytes);
        (index, hash, bytes)
    }

    /// Reassembles every slice a transfer produces, decompressing as it goes.
    fn drain(transfers: &mut Transfers, index: &ContentIndex) -> Vec<(ContentHash, Vec<u8>)> {
        let mut assembled: Vec<(ContentHash, Vec<u8>)> = Vec::new();
        loop {
            let slices = transfers.next_slices(index);
            if slices.is_empty() {
                return assembled;
            }
            for message in slices {
                let ServerMessage::ContentChunk {
                    hash,
                    offset,
                    total_len,
                    data,
                } = message
                else {
                    panic!("expected a ContentChunk, got {message:?}");
                };
                let plain = zstd::decode_all(data.as_slice()).expect("decompress");
                let entry = assembled.iter_mut().find(|(existing, _)| *existing == hash);
                match entry {
                    Some((_, buffer)) => {
                        assert_eq!(
                            buffer.len() as u64,
                            offset,
                            "slices must arrive in order with no gaps"
                        );
                        buffer.extend_from_slice(&plain);
                    }
                    None => {
                        assert_eq!(offset, 0, "the first slice of an item must be at offset 0");
                        assert!(total_len > 0 || plain.is_empty());
                        assembled.push((hash, plain));
                    }
                }
            }
        }
    }

    #[test]
    fn a_small_file_transfers_in_one_slice_and_round_trips() {
        let (index, hash, bytes) = index_with("small", 1024);
        let mut transfers = Transfers::new();
        assert_eq!(transfers.request(&[hash], &index), 1);

        let assembled = drain(&mut transfers, &index);
        assert_eq!(assembled.len(), 1);
        assert_eq!(assembled[0].0, hash);
        assert_eq!(
            assembled[0].1, bytes,
            "the bytes must survive the round trip"
        );
        assert_eq!(transfers.queued(), 0);
    }

    #[test]
    fn a_large_file_transfers_in_ordered_slices_and_round_trips() {
        // The reason for slicing at all. `drain` asserts the offsets line up
        // with no gaps, so a mis-stepped offset fails here rather than
        // producing a corrupt file on the client.
        let (index, hash, bytes) = index_with("large", SLICE_BYTES * 3 + 12_345);
        let mut transfers = Transfers::new();
        transfers.request(&[hash], &index);

        let assembled = drain(&mut transfers, &index);
        assert_eq!(assembled.len(), 1);
        assert_eq!(assembled[0].1.len(), bytes.len());
        assert_eq!(assembled[0].1, bytes);
    }

    #[test]
    fn an_empty_file_sends_one_slice_and_finishes() {
        // The infinite-loop case: offset and total_len are both zero, so a
        // naive `offset < total_len` check never fires and the item never
        // leaves the queue.
        let (index, hash, _) = index_with("empty", 0);
        let mut transfers = Transfers::new();
        transfers.request(&[hash], &index);

        let first = transfers.next_slices(&index);
        assert_eq!(first.len(), 1, "an empty file still sends one slice");
        assert_eq!(transfers.queued(), 0, "and then finishes");
        assert!(transfers.next_slices(&index).is_empty());
    }

    #[test]
    fn an_unknown_hash_is_refused_without_a_distinguishable_answer() {
        // Answering "no such content" differently from "here it is" turns the
        // server into an oracle for probing what a private mod pack holds.
        let (index, _, _) = index_with("unknown", 64);
        let mut transfers = Transfers::new();

        assert_eq!(transfers.request(&[[0xAB; 32]], &index), 0);
        assert_eq!(transfers.queued(), 0);
        assert!(
            transfers.next_slices(&index).is_empty(),
            "an unknown hash must produce no message at all"
        );
    }

    #[test]
    fn a_repeated_request_does_not_send_the_file_twice() {
        // Otherwise a client looping its request list is an amplifier: cheap to
        // send, expensive to serve.
        let (index, hash, _) = index_with("repeat", 4096);
        let mut transfers = Transfers::new();

        assert_eq!(transfers.request(&[hash], &index), 1);
        assert_eq!(
            transfers.request(&[hash, hash, hash], &index),
            0,
            "a hash already queued must be ignored"
        );

        let assembled = drain(&mut transfers, &index);
        assert_eq!(assembled.len(), 1);

        // And after it has been sent, asking again still does nothing.
        assert_eq!(transfers.request(&[hash], &index), 0);
        assert!(transfers.next_slices(&index).is_empty());
    }

    #[test]
    fn one_pass_sends_at_most_the_per_pass_budget() {
        // Bounds how much work a single request can cause at once, so a big
        // transfer is slow rather than a stall for everyone else.
        let (index, hash, _) = index_with("budget", SLICE_BYTES * 10);
        let mut transfers = Transfers::new();
        transfers.request(&[hash], &index);

        let slices = transfers.next_slices(&index);
        assert_eq!(slices.len(), SLICES_PER_PASS);
        assert!(transfers.queued() > 0, "the transfer should still be going");
    }

    #[test]
    fn a_compressed_slice_fits_the_protocol_chunk_limit() {
        // Incompressible data is the case that matters: zstd adds framing
        // rather than shrinking, so a slice sized at the protocol limit would
        // come out over it.
        let dir = scratch("incompressible");
        // Pseudo-random bytes that zstd cannot shrink. A fixed sequence rather
        // than randomness, so a failure reproduces.
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let bytes: Vec<u8> = (0..SLICE_BYTES * 2)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                u8::try_from(state & 0xFF).unwrap_or(0)
            })
            .collect();
        write(&dir, "noise.png", &bytes);

        let mut index = ContentIndex::new();
        index.add_mod("x", &dir).expect("index");
        let hash = hash_bytes(&bytes);

        let mut transfers = Transfers::new();
        transfers.request(&[hash], &index);

        loop {
            let slices = transfers.next_slices(&index);
            if slices.is_empty() {
                break;
            }
            for message in slices {
                let ServerMessage::ContentChunk { data, .. } = message else {
                    panic!("expected a ContentChunk");
                };
                assert!(
                    data.len() <= MAX_CONTENT_CHUNK_BYTES,
                    "a compressed slice of incompressible data was {} bytes, over the \
                     {MAX_CONTENT_CHUNK_BYTES}-byte protocol limit",
                    data.len()
                );
            }
        }
    }

    #[test]
    fn several_files_transfer_in_request_order() {
        let dir = scratch("several");
        write(&dir, "a.png", b"alpha");
        write(&dir, "b.png", b"beta");
        write(&dir, "c.png", b"gamma");
        let mut index = ContentIndex::new();
        index.add_mod("x", &dir).expect("index");

        let order = [
            hash_bytes(b"gamma"),
            hash_bytes(b"alpha"),
            hash_bytes(b"beta"),
        ];
        let mut transfers = Transfers::new();
        assert_eq!(transfers.request(&order, &index), 3);

        let assembled = drain(&mut transfers, &index);
        let hashes: Vec<ContentHash> = assembled.iter().map(|(hash, _)| *hash).collect();
        assert_eq!(hashes, order, "items must be served in the order requested");
    }

    #[test]
    fn the_quota_stops_a_client_pulling_without_bound() {
        let (index, hash, _) = index_with("quota", 4096);
        let mut transfers = Transfers::new();
        transfers.request(&[hash], &index);

        assert!(!transfers.over_quota());
        drain(&mut transfers, &index);
        assert_eq!(transfers.sent_bytes(), 4096);

        // Force the quota to its limit and confirm sending stops.
        transfers.sent_bytes = QUOTA_BYTES;
        transfers.seen.clear();
        transfers.request(&[hash], &index);
        assert!(transfers.over_quota());
        assert!(
            transfers.next_slices(&index).is_empty(),
            "a client over quota must not be served more"
        );
    }

    #[test]
    fn a_fresh_transfer_has_nothing_to_send() {
        let (index, _, _) = index_with("fresh", 16);
        let mut transfers = Transfers::new();
        assert!(transfers.next_slices(&index).is_empty());
        assert_eq!(transfers.sent_bytes(), 0);
    }
}
