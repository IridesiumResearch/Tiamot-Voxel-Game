// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Content addressing for server-pushed mod files.
//!
//! Every distributable file is identified by the `BLAKE3` hash of its bytes.
//! A client caches by hash, so the same texture shipped by fifty servers is
//! downloaded once, and a server never resends something a client already has.
//!
//! # Why hashes rather than names and versions
//!
//! A name plus a version is a *claim* that two files are the same. A hash is a
//! fact. Mods get edited without version bumps, servers run local patches, and
//! a cache keyed on `mymod-1.2.0/stone.png` would happily serve one server's
//! stone for another's. Keying on content makes that impossible, and makes the
//! "do I already have this?" question answerable without asking anyone.
//!
//! # What is distributable, and what is not
//!
//! **Only the file types a client needs to render and play** — see
//! [`is_distributable`]. Server-side Lua is not pushed: it may contain admin
//! logic, allowlists, or credentials, and a client has no use for it. The list
//! is an allowlist rather than a denylist on purpose, because a denylist gets a
//! new hole every time someone adds a file type.
//!
//! # Determinism
//!
//! The index is built in sorted path order, so the same mod directory produces
//! the same set fingerprint on every machine. Directory iteration order is not
//! stable across filesystems, and a fingerprint that depended on it would make
//! two identical servers look different to a client.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::proto::ContentHash;

/// Largest single file the server will index and push.
///
/// A mod shipping a 200 MB video is a mistake, not a feature, and finding out
/// at transfer time rather than at load time makes it the player's problem
/// instead of the operator's.
pub const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// Largest total content one mod may contribute.
pub const MAX_MOD_BYTES: u64 = 128 * 1024 * 1024;

/// File extensions a server will push to clients.
///
/// An **allowlist**. A denylist would grow a new hole every time a file type
/// was invented, and the failure mode is leaking a server's private files.
const DISTRIBUTABLE: [&str; 9] = [
    "png", "jpg", "jpeg", // textures
    "ogg", "wav", // audio
    "gltf", "glb", // models
    "json", "toml", // client-readable metadata
];

/// Whether a path is one clients receive.
///
/// Note what is **absent**: `.lua`. Server mod code can hold admin logic,
/// allowlists, or tokens, and a client has no use for it. Client-side scripts
/// are a separate mechanism with their own sandbox (charter rule 10), not
/// something that leaks out of the server mod directory by default.
#[must_use]
pub fn is_distributable(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .is_some_and(|ext| DISTRIBUTABLE.contains(&ext.as_str()))
}

/// Something went wrong building the content index.
#[derive(Debug, thiserror::Error)]
pub enum ContentError {
    /// A file could not be read.
    #[error("could not read `{path}`")]
    Read {
        /// Which file.
        path: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },

    /// A file is larger than [`MAX_FILE_BYTES`].
    #[error(
        "`{path}` is {len} bytes, over the {MAX_FILE_BYTES}-byte per-file limit. Shipping it \
         would make every joining player download it before they could play."
    )]
    FileTooLarge {
        /// Which file.
        path: PathBuf,
        /// Its size.
        len: u64,
    },

    /// A mod's total content is larger than [`MAX_MOD_BYTES`].
    #[error("mod `{mod_id}` has {len} bytes of content, over the {MAX_MOD_BYTES}-byte limit")]
    ModTooLarge {
        /// Which mod.
        mod_id: String,
        /// Its total size.
        len: u64,
    },
}

/// One distributable file.
#[derive(Debug, Clone)]
pub struct ContentItem {
    /// `BLAKE3` of the bytes.
    pub hash: ContentHash,
    /// Path relative to the mod directory, for diagnostics and client caching.
    pub relative_path: String,
    /// Which mod supplied it.
    pub mod_id: String,
    /// The bytes.
    pub bytes: Vec<u8>,
}

/// Every distributable file, keyed by hash.
#[derive(Debug, Default)]
pub struct ContentIndex {
    items: BTreeMap<ContentHash, ContentItem>,
    /// `(mod id, relative path)` → hash, for [`ContentIndex::hash_of`].
    ///
    /// Separate from `items` because deduplication makes `items` one-to-many in
    /// this direction: one entry can be the file several mods shipped.
    paths: BTreeMap<(String, String), ContentHash>,
    /// Per-mod content fingerprints, in load order.
    mod_hashes: Vec<(String, ContentHash)>,
}

impl ContentIndex {
    /// An empty index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Indexes one mod's directory.
    ///
    /// Returns the mod's content fingerprint: a hash over every file's hash and
    /// path, in sorted order. Two servers running the same mod produce the same
    /// fingerprint, so a client can tell at manifest time whether it already has
    /// everything.
    ///
    /// # Errors
    ///
    /// [`ContentError`] if a file cannot be read or a size limit is exceeded.
    pub fn add_mod(&mut self, mod_id: &str, dir: &Path) -> Result<ContentHash, ContentError> {
        // Sorted, so the fingerprint does not depend on directory iteration
        // order — which is not stable across filesystems, and would make two
        // identical servers look different to a client.
        let mut paths = Vec::new();
        collect(dir, dir, &mut paths)?;
        paths.sort();

        let mut total = 0u64;
        let mut fingerprint = blake3::Hasher::new();
        fingerprint.update(b"tiamot:mod-content:v1");
        fingerprint.update(mod_id.as_bytes());

        for relative in paths {
            let absolute = dir.join(&relative);
            let metadata = std::fs::metadata(&absolute).map_err(|source| ContentError::Read {
                path: absolute.clone(),
                source,
            })?;

            // Checked from metadata BEFORE the read. Reading first and then
            // complaining would mean a 2 GB file is loaded into memory to be
            // rejected.
            if metadata.len() > MAX_FILE_BYTES {
                return Err(ContentError::FileTooLarge {
                    path: absolute,
                    len: metadata.len(),
                });
            }
            total = total.saturating_add(metadata.len());
            if total > MAX_MOD_BYTES {
                return Err(ContentError::ModTooLarge {
                    mod_id: mod_id.to_owned(),
                    len: total,
                });
            }

            let bytes = std::fs::read(&absolute).map_err(|source| ContentError::Read {
                path: absolute,
                source,
            })?;
            let hash = hash_bytes(&bytes);

            // Path and hash both feed the fingerprint. Hashing only the
            // contents would make renaming a file invisible, and a client
            // would keep using the old name.
            let relative = relative.to_string_lossy().replace('\\', "/");
            fingerprint.update(relative.as_bytes());
            fingerprint.update(&hash);

            self.paths
                .insert((mod_id.to_owned(), relative.clone()), hash);

            // Two mods shipping a byte-identical file share one entry. That is
            // the point of content addressing, and it means the second one is
            // free.
            self.items.entry(hash).or_insert_with(|| ContentItem {
                hash,
                relative_path: relative,
                mod_id: mod_id.to_owned(),
                bytes,
            });
        }

        let fingerprint = *fingerprint.finalize().as_bytes();
        self.mod_hashes.push((mod_id.to_owned(), fingerprint));
        Ok(fingerprint)
    }

    /// Looks up an item by hash.
    #[must_use]
    pub fn get(&self, hash: &ContentHash) -> Option<&ContentItem> {
        self.items.get(hash)
    }

    /// The hash of one mod's file, by the path the mod refers to it by.
    ///
    /// The reverse direction of the index, and the one a *registration* needs:
    /// a mod names `"textures/white.png"` and the client is told a hash. Doing
    /// it this way round is what keeps a mod-supplied string from ever reaching
    /// the filesystem — the lookup succeeds only for paths the index found by
    /// walking that mod's own directory, so traversal has nothing to match.
    ///
    /// Keyed by mod **and** path, not by path alone. Deduplication means two
    /// mods shipping a byte-identical file share one [`ContentItem`], and that
    /// item can only record one owner — so searching the items for a matching
    /// `mod_id` would fail for whichever mod happened to be indexed second.
    #[must_use]
    pub fn hash_of(&self, mod_id: &str, relative_path: &str) -> Option<ContentHash> {
        self.paths
            .get(&(mod_id.to_owned(), relative_path.replace('\\', "/")))
            .copied()
    }

    /// How many distinct items are indexed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the index is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Total bytes across every item.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.items
            .values()
            .map(|item| item.bytes.len() as u64)
            .sum()
    }

    /// Per-mod fingerprints, in the order mods were added.
    #[must_use]
    pub fn mod_hashes(&self) -> &[(String, ContentHash)] {
        &self.mod_hashes
    }

    /// Every hash, in a stable order.
    #[must_use]
    pub fn hashes(&self) -> Vec<ContentHash> {
        self.items.keys().copied().collect()
    }
}

/// `BLAKE3` of some bytes, domain-separated for content addressing.
#[must_use]
pub fn hash_bytes(bytes: &[u8]) -> ContentHash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"tiamot:content:v1");
    hasher.update(bytes);
    *hasher.finalize().as_bytes()
}

/// Collects distributable files under `dir`, as paths relative to `root`.
fn collect(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), ContentError> {
    let entries = std::fs::read_dir(dir).map_err(|source| ContentError::Read {
        path: dir.to_path_buf(),
        source,
    })?;

    for entry in entries {
        let entry = entry.map_err(|source| ContentError::Read {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();

        // `file_type` rather than `metadata`: it does not follow symlinks, so a
        // link pointing at /etc is not silently indexed and served. A mod
        // directory is not necessarily written by someone trustworthy.
        let file_type = entry.file_type().map_err(|source| ContentError::Read {
            path: path.clone(),
            source,
        })?;

        if file_type.is_symlink() {
            // Skipped rather than followed. Following one is how a mod
            // directory becomes a way to read arbitrary files off the server.
            continue;
        }
        if file_type.is_dir() {
            collect(root, &path, out)?;
        } else if file_type.is_file()
            && is_distributable(&path)
            && let Ok(relative) = path.strip_prefix(root)
        {
            out.push(relative.to_path_buf());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("tiamot-content-tests").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn write(dir: &Path, relative: &str, bytes: &[u8]) {
        let path = dir.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parent");
        }
        std::fs::write(path, bytes).expect("write");
    }

    #[test]
    fn a_path_resolves_to_a_hash_even_when_two_mods_ship_the_same_file() {
        // The lookup a block-texture registration needs: a mod names a path and
        // the client is told a hash. Deduplication makes the item table
        // one-to-many in this direction -- one entry is the file BOTH mods
        // shipped, and it can only record one owner -- so the reverse map is
        // keyed by mod and path rather than searched for a matching owner.
        let first = scratch("shared-first");
        let second = scratch("shared-second");
        write(&first, "textures/white.png", b"identical bytes");
        write(&second, "textures/white.png", b"identical bytes");

        let mut index = ContentIndex::new();
        index.add_mod("first", &first).expect("index");
        index.add_mod("second", &second).expect("index");

        assert_eq!(index.len(), 1, "identical files must share one entry");
        let a = index
            .hash_of("first", "textures/white.png")
            .expect("the first mod's path resolves");
        let b = index
            .hash_of("second", "textures/white.png")
            .expect("and so must the second mod's, which was deduplicated away");
        assert_eq!(a, b);
    }

    #[test]
    fn a_path_outside_the_mod_directory_resolves_to_nothing() {
        // Not because it is checked here, but because it cannot match: the map
        // holds only paths found by walking the mod's own directory. This is
        // what makes it safe for the key to be a mod-supplied string.
        let dir = scratch("traversal");
        write(&dir, "textures/white.png", b"texture");

        let mut index = ContentIndex::new();
        index.add_mod("x", &dir).expect("index");

        assert!(index.hash_of("x", "../../etc/passwd").is_none());
        assert!(index.hash_of("x", "/etc/passwd").is_none());
        assert!(
            index.hash_of("y", "textures/white.png").is_none(),
            "another mod's path must not resolve"
        );
    }

    #[test]
    fn only_distributable_extensions_are_indexed() {
        // The allowlist is the security boundary: server Lua can hold admin
        // logic, allowlists, or tokens, and a client has no use for it.
        let dir = scratch("extensions");
        write(&dir, "stone.png", b"texture");
        write(&dir, "step.ogg", b"audio");
        write(&dir, "init.lua", b"-- secrets");
        write(&dir, "admin_tokens.txt", b"hunter2");
        write(&dir, "mod.toml", b"id = 'x'");

        let mut index = ContentIndex::new();
        index.add_mod("x", &dir).expect("index");

        let paths: Vec<&str> = index
            .items
            .values()
            .map(|item| item.relative_path.as_str())
            .collect();
        assert!(paths.contains(&"stone.png"));
        assert!(paths.contains(&"step.ogg"));
        assert!(paths.contains(&"mod.toml"));
        // Checked through `is_distributable` rather than by string suffix, so
        // the assertion tests the real rule and not a second copy of it.
        assert!(
            !paths.iter().any(|p| is_distributable(Path::new(p))
                && Path::new(p).extension().is_some_and(|e| e == "lua")),
            "server Lua must never be pushed to clients: {paths:?}"
        );
        assert!(
            paths.iter().all(|p| is_distributable(Path::new(p))),
            "every indexed path must be distributable: {paths:?}"
        );
        assert_eq!(paths.len(), 3, "exactly the three allowed files: {paths:?}");
    }

    #[test]
    fn extension_matching_is_case_insensitive() {
        // Otherwise `STONE.PNG` is silently not shipped, and the texture is
        // missing on exactly the machines whose tooling upper-cases names.
        assert!(is_distributable(Path::new("a/STONE.PNG")));
        assert!(is_distributable(Path::new("a/Stone.Png")));
        assert!(!is_distributable(Path::new("a/init.LUA")));
    }

    #[test]
    fn nested_directories_are_indexed() {
        let dir = scratch("nested");
        write(&dir, "textures/blocks/stone.png", b"deep");
        let mut index = ContentIndex::new();
        index.add_mod("x", &dir).expect("index");

        assert_eq!(index.len(), 1);
        assert_eq!(
            index.items.values().next().expect("item").relative_path,
            "textures/blocks/stone.png"
        );
    }

    #[test]
    fn the_fingerprint_is_stable_and_order_independent() {
        // Directory iteration order is not stable across filesystems. A
        // fingerprint that depended on it would make two identical servers look
        // different to a client, and every player would re-download everything.
        let first = scratch("fingerprint-a");
        write(&first, "a.png", b"one");
        write(&first, "b.png", b"two");
        write(&first, "c.png", b"three");

        let second = scratch("fingerprint-b");
        // Written in a different order.
        write(&second, "c.png", b"three");
        write(&second, "a.png", b"one");
        write(&second, "b.png", b"two");

        let mut index_a = ContentIndex::new();
        let mut index_b = ContentIndex::new();
        assert_eq!(
            index_a.add_mod("x", &first).expect("index"),
            index_b.add_mod("x", &second).expect("index"),
            "the same files must fingerprint the same however they were written"
        );
    }

    #[test]
    fn changing_a_file_changes_the_fingerprint() {
        let dir = scratch("fingerprint-change");
        write(&dir, "a.png", b"before");
        let mut index = ContentIndex::new();
        let before = index.add_mod("x", &dir).expect("index");

        write(&dir, "a.png", b"after");
        let mut index = ContentIndex::new();
        let after = index.add_mod("x", &dir).expect("index");

        assert_ne!(before, after, "an edited file must change the fingerprint");
    }

    #[test]
    fn renaming_a_file_changes_the_fingerprint() {
        // The fingerprint hashes paths as well as contents. Hashing only
        // contents would make a rename invisible, and a client would go on
        // asking for the old name.
        let dir = scratch("fingerprint-rename");
        write(&dir, "old.png", b"same bytes");
        let mut index = ContentIndex::new();
        let before = index.add_mod("x", &dir).expect("index");

        std::fs::remove_file(dir.join("old.png")).expect("remove");
        write(&dir, "new.png", b"same bytes");
        let mut index = ContentIndex::new();
        let after = index.add_mod("x", &dir).expect("index");

        assert_ne!(before, after, "a rename must change the fingerprint");
    }

    #[test]
    fn identical_files_in_two_mods_share_one_entry() {
        // The point of content addressing: the second copy is free.
        let first = scratch("dedup-a");
        let second = scratch("dedup-b");
        write(&first, "stone.png", b"identical bytes");
        write(&second, "also_stone.png", b"identical bytes");

        let mut index = ContentIndex::new();
        index.add_mod("a", &first).expect("index");
        index.add_mod("b", &second).expect("index");

        assert_eq!(index.len(), 1, "byte-identical files must share an entry");
        assert_eq!(index.total_bytes(), "identical bytes".len() as u64);
    }

    #[test]
    fn different_mods_get_different_fingerprints_for_the_same_files() {
        // The mod id is in the fingerprint, so two mods shipping identical
        // content are still distinguishable in the manifest.
        let first = scratch("modid-a");
        let second = scratch("modid-b");
        write(&first, "x.png", b"same");
        write(&second, "x.png", b"same");

        let mut index = ContentIndex::new();
        let a = index.add_mod("alpha", &first).expect("index");
        let b = index.add_mod("beta", &second).expect("index");
        assert_ne!(a, b);
    }

    #[test]
    fn an_oversized_file_is_refused_before_it_is_read() {
        // Reading first and complaining afterwards would load the whole thing
        // into memory to reject it.
        let dir = scratch("too-big");
        let path = dir.join("huge.png");
        let file = std::fs::File::create(&path).expect("create");
        file.set_len(MAX_FILE_BYTES + 1).expect("grow");
        drop(file);

        let mut index = ContentIndex::new();
        let err = index.add_mod("x", &dir).expect_err("must refuse");
        assert!(matches!(err, ContentError::FileTooLarge { .. }), "{err}");
        assert!(
            err.to_string().contains("before they could play"),
            "the message should say why it matters: {err}"
        );
    }

    #[test]
    fn an_empty_directory_indexes_to_nothing() {
        let dir = scratch("empty");
        let mut index = ContentIndex::new();
        index.add_mod("x", &dir).expect("index");
        assert!(index.is_empty());
        assert_eq!(index.total_bytes(), 0);
    }

    #[test]
    fn a_hash_identifies_its_bytes() {
        assert_eq!(hash_bytes(b"abc"), hash_bytes(b"abc"));
        assert_ne!(hash_bytes(b"abc"), hash_bytes(b"abd"));
        assert_ne!(
            hash_bytes(b"abc"),
            *blake3::hash(b"abc").as_bytes(),
            "content hashes must be domain-separated from a bare BLAKE3"
        );
    }

    #[test]
    fn lookup_by_hash_returns_the_right_bytes() {
        let dir = scratch("lookup");
        write(&dir, "a.png", b"alpha bytes");
        write(&dir, "b.png", b"beta bytes");

        let mut index = ContentIndex::new();
        index.add_mod("x", &dir).expect("index");

        let alpha = hash_bytes(b"alpha bytes");
        assert_eq!(
            index.get(&alpha).expect("indexed").bytes,
            b"alpha bytes".to_vec()
        );
        assert!(
            index.get(&[0u8; 32]).is_none(),
            "an unknown hash resolves to nothing"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_skipped_rather_than_followed() {
        // Following one is how a mod directory becomes a way to read arbitrary
        // files off the server, and mod directories are not necessarily written
        // by someone trustworthy.
        let dir = scratch("symlink");
        let secret = dir.join("..").join("secret.png");
        std::fs::write(&secret, b"not for clients").expect("write secret");
        std::os::unix::fs::symlink(&secret, dir.join("innocent.png")).expect("symlink");
        write(&dir, "real.png", b"fine");

        let mut index = ContentIndex::new();
        index.add_mod("x", &dir).expect("index");

        assert_eq!(index.len(), 1, "only the real file should be indexed");
        assert!(
            index.get(&hash_bytes(b"not for clients")).is_none(),
            "a symlinked file must never be served"
        );
    }
}
