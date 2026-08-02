// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Trust-on-first-use for server certificates.
//!
//! # There is no certificate authority here, and that is deliberate
//!
//! Servers are self-signed (Task 06). A CA would mean either a central registry
//! of who may run a server — which is the opposite of what this engine is for —
//! or accepting any certificate at all, which makes the fingerprint binding in
//! the auth handshake mean nothing.
//!
//! So: **the first connection to an address is trusted and remembered, and every
//! connection after it must match.** This is the SSH model, and it has the same
//! honest limit — the first connection is the one you cannot verify. What it
//! does buy is that an interception has to be there from the very first time you
//! ever connected, and cannot start later.
//!
//! # A mismatch is a refusal, not a prompt
//!
//! [`TrustStore::check`] returns a [`Trust`] and does not decide anything. A
//! changed fingerprint is either an operator who regenerated a certificate or
//! somebody sitting in the middle, and a client cannot tell which — so the
//! caller refuses the connection and says what to do about it. Silently
//! re-pinning would make the whole mechanism decorative.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// What is known about a server's certificate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    /// Never seen. The caller should connect and then [`TrustStore::remember`].
    FirstUse,
    /// Seen before, and it matches.
    Known,
    /// Seen before, and it does **not** match.
    Changed {
        /// What was pinned last time.
        expected: [u8; 32],
    },
}

/// Something went wrong reading or writing the store.
#[derive(Debug, thiserror::Error)]
pub enum TrustError {
    /// The file could not be read or written.
    #[error("known-hosts file `{path}` is unusable")]
    Io {
        /// Which file.
        path: PathBuf,
        /// Why.
        #[source]
        source: std::io::Error,
    },
}

/// Remembered server fingerprints, keyed by the address they were seen at.
///
/// Keyed by address rather than by name because there are no names: a server is
/// a socket address, and that is what a player typed.
#[derive(Debug, Clone, Default)]
pub struct TrustStore {
    path: PathBuf,
    known: BTreeMap<String, [u8; 32]>,
}

impl TrustStore {
    /// Loads a store, treating an absent or unreadable file as empty.
    ///
    /// A corrupt line is skipped rather than fatal. The file is plain text that
    /// people edit, and one bad line should cost one pinned server, not the
    /// ability to play.
    ///
    /// # Errors
    ///
    /// Never fails on a missing file — see above. [`TrustError::Io`] is
    /// reserved for [`TrustStore::save`].
    #[must_use]
    pub fn load(path: &Path) -> Self {
        let mut known = BTreeMap::new();
        if let Ok(text) = std::fs::read_to_string(path) {
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let mut parts = line.split_whitespace();
                let (Some(address), Some(hex)) = (parts.next(), parts.next()) else {
                    continue;
                };
                if let Some(fingerprint) = from_hex(hex) {
                    known.insert(address.to_owned(), fingerprint);
                } else {
                    tracing::warn!(line, "skipping an unreadable known-hosts entry");
                }
            }
        }
        Self {
            path: path.to_path_buf(),
            known,
        }
    }

    /// What is known about this address.
    #[must_use]
    pub fn check(&self, address: &str, presented: &[u8; 32]) -> Trust {
        match self.known.get(address) {
            None => Trust::FirstUse,
            Some(expected) if expected == presented => Trust::Known,
            Some(expected) => Trust::Changed {
                expected: *expected,
            },
        }
    }

    /// The fingerprint pinned for an address, if any.
    #[must_use]
    pub fn pinned(&self, address: &str) -> Option<[u8; 32]> {
        self.known.get(address).copied()
    }

    /// Pins a fingerprint, replacing whatever was there.
    ///
    /// Deliberately unconditional: the *decision* to re-pin belongs to the
    /// caller, who is the only one who can ask a human. A method that refused
    /// to overwrite would push that decision into a place with no way to
    /// explain itself.
    pub fn remember(&mut self, address: &str, fingerprint: [u8; 32]) {
        self.known.insert(address.to_owned(), fingerprint);
    }

    /// Forgets an address.
    pub fn forget(&mut self, address: &str) -> bool {
        self.known.remove(address).is_some()
    }

    /// Writes the store back out.
    ///
    /// # Errors
    ///
    /// [`TrustError::Io`] if the file cannot be written.
    pub fn save(&self) -> Result<(), TrustError> {
        let mut text = String::from(
            "# Tiamot known servers. One line per address:\n\
             #   <address> <BLAKE3 of the server certificate, hex>\n\
             #\n\
             # A server whose fingerprint stops matching is refused. If you know why it\n\
             # changed -- an operator regenerated it, say -- delete its line and reconnect.\n",
        );
        for (address, fingerprint) in &self.known {
            text.push_str(address);
            text.push(' ');
            text.push_str(&to_hex(fingerprint));
            text.push('\n');
        }

        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| TrustError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        std::fs::write(&self.path, text).map_err(|source| TrustError::Io {
            path: self.path.clone(),
            source,
        })
    }
}

/// A fingerprint as lowercase hex.
#[must_use]
pub fn to_hex(fingerprint: &[u8; 32]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in fingerprint {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0F) as usize] as char);
    }
    out
}

/// A fingerprint from hex, or `None` if it is not 64 hex digits.
fn from_hex(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("tiamot-trust-tests");
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join(format!("{name}-known-hosts"));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn the_first_connection_to_an_address_is_first_use() {
        let store = TrustStore::load(&scratch("first"));
        assert_eq!(store.check("example:47811", &[1u8; 32]), Trust::FirstUse);
    }

    #[test]
    fn a_remembered_fingerprint_survives_a_restart() {
        // The whole point. A store that forgot on exit would make every
        // connection a first use, which is trust-on-first-use in name only.
        let path = scratch("persist");
        let mut store = TrustStore::load(&path);
        store.remember("example:47811", [7u8; 32]);
        store.save().expect("save");

        let reloaded = TrustStore::load(&path);
        assert_eq!(reloaded.check("example:47811", &[7u8; 32]), Trust::Known);
    }

    #[test]
    fn a_changed_fingerprint_reports_what_was_expected() {
        // The caller has to be able to tell the player what changed, or the
        // refusal is unactionable.
        let path = scratch("changed");
        let mut store = TrustStore::load(&path);
        store.remember("example:47811", [7u8; 32]);

        assert_eq!(
            store.check("example:47811", &[8u8; 32]),
            Trust::Changed {
                expected: [7u8; 32]
            }
        );
    }

    #[test]
    fn two_addresses_are_two_servers() {
        let mut store = TrustStore::load(&scratch("two"));
        store.remember("a:47811", [1u8; 32]);
        store.remember("b:47811", [2u8; 32]);

        assert_eq!(store.check("a:47811", &[1u8; 32]), Trust::Known);
        assert_eq!(store.check("b:47811", &[2u8; 32]), Trust::Known);
        assert!(matches!(
            store.check("a:47811", &[2u8; 32]),
            Trust::Changed { .. }
        ));
    }

    #[test]
    fn one_corrupt_line_costs_one_server_rather_than_the_file() {
        // This is a plain-text file that people edit. A parser that gave up on
        // the first bad line would turn a typo into "you cannot connect to
        // anything".
        let path = scratch("corrupt");
        std::fs::write(
            &path,
            "# a comment\n\
             good:47811 "
                .to_owned()
                + &to_hex(&[3u8; 32])
                + "\n\
             bad:47811 not-hex\n\
             short:47811 abcd\n\
             \n",
        )
        .expect("write");

        let store = TrustStore::load(&path);
        assert_eq!(store.check("good:47811", &[3u8; 32]), Trust::Known);
        assert_eq!(store.check("bad:47811", &[0u8; 32]), Trust::FirstUse);
        assert_eq!(store.check("short:47811", &[0u8; 32]), Trust::FirstUse);
    }

    #[test]
    fn hex_round_trips() {
        let fingerprint = [0x0Au8; 32];
        assert_eq!(from_hex(&to_hex(&fingerprint)), Some(fingerprint));
        assert_eq!(from_hex("zz"), None);
        assert_eq!(from_hex(&"a".repeat(63)), None);
    }

    #[test]
    fn forgetting_an_address_makes_it_first_use_again() {
        // The remedy for a legitimately regenerated certificate, and the reason
        // the saved file explains it in a comment.
        let mut store = TrustStore::load(&scratch("forget"));
        store.remember("example:47811", [4u8; 32]);
        assert!(store.forget("example:47811"));
        assert_eq!(store.check("example:47811", &[5u8; 32]), Trust::FirstUse);
    }
}
