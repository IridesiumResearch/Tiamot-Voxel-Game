// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! A content-addressed cache for server-pushed assets.
//!
//! # Why hashes make a cache trivial
//!
//! Every file a server pushes is named by the `BLAKE3` of its bytes, so the
//! same texture shipped by fifty servers is downloaded once and shared. There
//! is no invalidation problem, because a changed file is a different name.
//!
//! # The hash is checked on the way in AND on the way out
//!
//! Charter rule 14: assets from a server are hostile input. The hash is the one
//! part of a server's claim a client can verify for itself, so:
//!
//! - **On store**, bytes that do not hash to the name they were requested under
//!   are refused. A server that sent something other than what was asked for is
//!   caught here rather than by the decoder it was aimed at.
//! - **On load**, the file is re-hashed. A cache lives on disk, where a
//!   half-written file survives a power cut and something else on the machine
//!   can write into it; trusting the filename would make the cache a way to
//!   feed a client bytes no server ever sent.
//!
//! Re-hashing costs a few microseconds against a disk read that costs
//! milliseconds. There is no version of this worth optimising away.

use std::path::{Path, PathBuf};

use tiamot_core::proto::ContentHash;

/// Largest cached item, in bytes.
///
/// Matches the engine's per-file push limit, so anything a legitimate server
/// can send fits and anything larger is a file this cache did not put there.
pub const MAX_ITEM_BYTES: u64 = tiamot_core::content::MAX_FILE_BYTES;

/// Something went wrong reading or writing the cache.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    /// The cache directory could not be created or read.
    #[error("content cache at `{path}` is unusable")]
    Io {
        /// Which path.
        path: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },

    /// Stored bytes did not hash to the name they were stored under.
    #[error(
        "content did not match the hash it was requested by. This is the one part of a server's \
         claim a client can check for itself, so it is refused rather than decoded."
    )]
    HashMismatch,

    /// A cached file is larger than anything a server may push.
    #[error("cached item is {len} bytes, over the {MAX_ITEM_BYTES}-byte limit")]
    TooLarge {
        /// Its size.
        len: u64,
    },
}

/// A directory of content-addressed files.
#[derive(Debug, Clone)]
pub struct ContentCache {
    root: PathBuf,
}

impl ContentCache {
    /// Opens (and creates) a cache directory.
    ///
    /// # Errors
    ///
    /// [`CacheError::Io`] if the directory cannot be created.
    pub fn open(root: &Path) -> Result<Self, CacheError> {
        std::fs::create_dir_all(root).map_err(|source| CacheError::Io {
            path: root.to_path_buf(),
            source,
        })?;
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    /// Where this cache lives.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The path a hash is stored at.
    ///
    /// Hex rather than base64: a cache directory is something people look at
    /// when something has gone wrong, and base64 is case-sensitive on a
    /// filesystem that may not be.
    #[must_use]
    pub fn path_of(&self, hash: &ContentHash) -> PathBuf {
        self.root.join(crate::trust::to_hex(hash))
    }

    /// Reads a cached item, verifying its contents.
    ///
    /// Returns `None` if it is absent, too large, or does not match its name —
    /// all three mean "fetch it again", and none is worth an error at the call
    /// site. A corrupt entry is deleted on the way out so it is not re-checked
    /// on every join.
    #[must_use]
    pub fn get(&self, hash: &ContentHash) -> Option<Vec<u8>> {
        let path = self.path_of(hash);
        let metadata = std::fs::metadata(&path).ok()?;
        if metadata.len() > MAX_ITEM_BYTES {
            let _ = std::fs::remove_file(&path);
            return None;
        }
        let bytes = std::fs::read(&path).ok()?;

        if tiamot_core::content::hash_bytes(&bytes) == *hash {
            Some(bytes)
        } else {
            // Something wrote into the cache that was not this client storing a
            // verified download. Removing it is the only sensible response:
            // keeping it means re-reading and re-rejecting it forever.
            tracing::warn!(
                path = %path.display(),
                "a cached asset does not match its own hash and has been discarded"
            );
            let _ = std::fs::remove_file(&path);
            None
        }
    }

    /// Whether an item is present and intact.
    #[must_use]
    pub fn contains(&self, hash: &ContentHash) -> bool {
        self.get(hash).is_some()
    }

    /// Stores an item, refusing bytes that do not match the hash.
    ///
    /// # Errors
    ///
    /// [`CacheError::HashMismatch`] if the bytes are not what was asked for,
    /// [`CacheError::TooLarge`] if they exceed [`MAX_ITEM_BYTES`], or
    /// [`CacheError::Io`] on a write failure.
    pub fn put(&self, hash: &ContentHash, bytes: &[u8]) -> Result<(), CacheError> {
        if bytes.len() as u64 > MAX_ITEM_BYTES {
            return Err(CacheError::TooLarge {
                len: bytes.len() as u64,
            });
        }
        if tiamot_core::content::hash_bytes(bytes) != *hash {
            return Err(CacheError::HashMismatch);
        }

        let path = self.path_of(hash);
        // Write beside, then rename. A cache entry must never be observable
        // half-written: the next run would read a truncated file, and because
        // it is content-addressed, "truncated" and "a different file" are
        // indistinguishable by name. Rename is atomic on every platform this
        // engine targets.
        let temporary = path.with_extension("partial");
        std::fs::write(&temporary, bytes).map_err(|source| CacheError::Io {
            path: temporary.clone(),
            source,
        })?;
        std::fs::rename(&temporary, &path).map_err(|source| {
            let _ = std::fs::remove_file(&temporary);
            CacheError::Io {
                path: path.clone(),
                source,
            }
        })?;
        Ok(())
    }

    /// Which of these hashes are not already held.
    ///
    /// The list a client asks a server for. Ordered and deduplicated so the
    /// request is a property of what is missing rather than of the order the
    /// material table happened to be walked in.
    #[must_use]
    pub fn missing(&self, wanted: &[ContentHash]) -> Vec<ContentHash> {
        let mut missing: Vec<ContentHash> = wanted
            .iter()
            .filter(|hash| !self.contains(hash))
            .copied()
            .collect();
        missing.sort_unstable();
        missing.dedup();
        missing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("tiamot-cache-tests").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn cache(name: &str) -> ContentCache {
        ContentCache::open(&scratch(name)).expect("open cache")
    }

    #[test]
    fn an_item_round_trips() {
        let cache = cache("round-trip");
        let bytes = b"a texture, more or less".to_vec();
        let hash = tiamot_core::content::hash_bytes(&bytes);

        assert!(!cache.contains(&hash));
        cache.put(&hash, &bytes).expect("store");
        assert_eq!(cache.get(&hash).as_deref(), Some(bytes.as_slice()));
    }

    #[test]
    fn bytes_that_do_not_match_their_hash_are_refused() {
        // The hash is the only part of a server's claim a client can check. A
        // cache that stored whatever it was handed would make it worthless.
        let cache = cache("mismatch");
        let hash = tiamot_core::content::hash_bytes(b"what was asked for");

        let err = cache
            .put(&hash, b"something else entirely")
            .expect_err("must refuse");
        assert!(matches!(err, CacheError::HashMismatch), "got {err}");
        assert!(!cache.contains(&hash), "and must not have stored it");
    }

    #[test]
    fn a_tampered_cache_entry_is_discarded_rather_than_served() {
        // A cache lives on disk, where anything else on the machine can write
        // into it. Trusting the filename would turn the cache into a way to
        // feed a client bytes no server ever sent.
        let cache = cache("tampered");
        let bytes = b"the real thing".to_vec();
        let hash = tiamot_core::content::hash_bytes(&bytes);
        cache.put(&hash, &bytes).expect("store");

        std::fs::write(cache.path_of(&hash), b"not the real thing").expect("tamper");

        assert!(
            cache.get(&hash).is_none(),
            "a tampered entry must not be served"
        );
        assert!(
            !cache.path_of(&hash).exists(),
            "and must be removed, or it is re-rejected on every join"
        );
    }

    #[test]
    fn an_oversized_entry_is_refused_from_its_length() {
        let cache = cache("oversized");
        let bytes = vec![0u8; MAX_ITEM_BYTES as usize + 1];
        let hash = tiamot_core::content::hash_bytes(&bytes);

        let err = cache.put(&hash, &bytes).expect_err("must refuse");
        assert!(matches!(err, CacheError::TooLarge { .. }), "got {err}");
    }

    #[test]
    fn a_partial_write_is_never_visible_under_the_real_name() {
        // Content addressing cannot tell "truncated" from "a different file",
        // so a half-written entry would be indistinguishable from a valid one
        // for some other content. Write-then-rename is what prevents it.
        let cache = cache("atomic");
        let bytes = b"content".to_vec();
        let hash = tiamot_core::content::hash_bytes(&bytes);
        cache.put(&hash, &bytes).expect("store");

        let leftovers: Vec<_> = std::fs::read_dir(cache.root())
            .expect("read dir")
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "partial"))
            .collect();
        assert!(leftovers.is_empty(), "a temporary file was left behind");
    }

    #[test]
    fn missing_reports_only_what_is_absent_and_says_it_once() {
        let cache = cache("missing");
        let held = b"already have this".to_vec();
        let held_hash = tiamot_core::content::hash_bytes(&held);
        cache.put(&held_hash, &held).expect("store");
        let wanted_hash = tiamot_core::content::hash_bytes(b"do not have this");

        let missing = cache.missing(&[held_hash, wanted_hash, wanted_hash, held_hash]);
        assert_eq!(missing, vec![wanted_hash]);
    }

    #[test]
    fn a_path_is_lowercase_hex_of_the_whole_hash() {
        // People read this directory when something has gone wrong. A truncated
        // name would collide; base64 would be case-sensitive on a filesystem
        // that may not be.
        let cache = cache("naming");
        let hash = [0xABu8; 32];
        let name = cache
            .path_of(&hash)
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
            .expect("a file name");
        assert_eq!(name, "ab".repeat(32));
    }
}
