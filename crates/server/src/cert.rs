// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The server's TLS identity, and the fingerprint clients pin.
//!
//! # There is no certificate authority
//!
//! Tiamot servers are run by anyone, on any address, usually without a domain
//! name. A CA-issued certificate would require every operator to own a domain
//! and renew a certificate to host a game server, which is not a trade anyone
//! would take. So certificates are self-signed and trust is
//! **trust-on-first-use**: a client remembers the fingerprint it saw the first
//! time it connected and refuses a different one afterwards.
//!
//! What that does and does not buy:
//!
//! - It **does** stop a passive eavesdropper reading or altering traffic, and
//!   it stops an attacker impersonating a server you have connected to before.
//! - It does **not** authenticate a server you have never seen. The first
//!   connection is trusted blindly, exactly as SSH's first connection is.
//!
//! The join handshake leans on the same fingerprint: a client signs
//! `(nonce ‖ cert fingerprint ‖ protocol version)`, so a signature captured on
//! one server cannot be relayed to another. That is what makes the blind first
//! connection tolerable — an attacker who intercepts it gets a signature that
//! is worthless anywhere else.
//!
//! # The certificate must be stable across restarts
//!
//! A freshly generated certificate on every boot would change the fingerprint
//! every time, and TOFU would reject every returning player. So the key is
//! written to the world directory and reused. Losing it is not fatal but is
//! disruptive: every client has to re-pin, which is indistinguishable from an
//! attack from the client's side.

use std::path::{Path, PathBuf};

use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

/// File holding the server's long-lived TLS private key, inside the world dir.
const KEY_FILE: &str = "server-key.pem";
/// File holding the matching self-signed certificate.
const CERT_FILE: &str = "server-cert.pem";

/// The name the certificate is issued for.
///
/// Meaningless under TOFU — nothing checks it, because there is no CA and no
/// domain. It exists because a certificate must carry *some* subject.
const SUBJECT: &str = "tiamot-server";

/// Something went wrong producing or loading the server certificate.
#[derive(Debug, thiserror::Error)]
pub enum CertError {
    /// A certificate file could not be read or written.
    #[error("could not access `{path}`")]
    Io {
        /// Which file.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },

    /// Generation failed.
    #[error("could not generate a server certificate")]
    Generate(#[source] rcgen::Error),

    /// The stored certificate or key could not be parsed.
    #[error(
        "the stored server certificate at `{path}` is unreadable: {reason}. Delete it and the \
         matching key to generate a new one — every client will have to re-pin the server's \
         fingerprint, which looks identical to an attack from their side, so say so publicly \
         before doing it."
    )]
    Unreadable {
        /// Which file.
        path: PathBuf,
        /// Why it could not be read.
        reason: String,
    },
}

/// A server's TLS identity and the fingerprint clients pin.
///
/// Not `Clone`: it holds a private key, and the fewer copies of that exist the
/// better. Everything downstream needs only the [`fingerprint`](Self::fingerprint),
/// which is `Copy`.
#[derive(Debug)]
pub struct ServerCert {
    /// The DER-encoded certificate chain (one self-signed certificate).
    pub chain: Vec<CertificateDer<'static>>,
    /// The private key.
    pub key: PrivateKeyDer<'static>,
    /// `BLAKE3` of the DER certificate.
    ///
    /// This is what a client pins and what the join handshake signs over.
    pub fingerprint: [u8; 32],
}

impl ServerCert {
    /// Loads the server's certificate, generating one on first run.
    ///
    /// # Errors
    ///
    /// [`CertError`] if the files exist but cannot be read, or if generation
    /// fails.
    pub fn load_or_create(world_dir: &Path) -> Result<Self, CertError> {
        let key_path = world_dir.join(KEY_FILE);
        let cert_path = world_dir.join(CERT_FILE);

        if key_path.exists() && cert_path.exists() {
            return Self::load(&cert_path, &key_path);
        }

        // Either both or neither. One file without the other means a partial
        // write or a half-finished manual edit, and silently regenerating would
        // change the fingerprint without anyone noticing.
        Self::create(world_dir, &cert_path, &key_path)
    }

    fn create(world_dir: &Path, cert_path: &Path, key_path: &Path) -> Result<Self, CertError> {
        std::fs::create_dir_all(world_dir).map_err(|source| CertError::Io {
            path: world_dir.to_path_buf(),
            source,
        })?;

        let generated = rcgen::generate_simple_self_signed(vec![SUBJECT.to_owned()])
            .map_err(CertError::Generate)?;

        let cert_pem = generated.cert.pem();
        let key_pem = generated.signing_key.serialize_pem();

        write(cert_path, cert_pem.as_bytes())?;
        write(key_path, key_pem.as_bytes())?;
        // The private key is a credential. Same reasoning as the player key
        // file: on a shared machine, world-readable is the difference between
        // "my server" and "anyone's server".
        restrict_permissions(key_path)?;

        let der = generated.cert.der().clone();
        let fingerprint = fingerprint_of(&der);
        Ok(Self {
            chain: vec![der],
            key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
                generated.signing_key.serialize_der(),
            )),
            fingerprint,
        })
    }

    fn load(cert_path: &Path, key_path: &Path) -> Result<Self, CertError> {
        let cert_pem = read(cert_path)?;
        let key_pem = read(key_path)?;

        let der = pem_to_der(&cert_pem, "CERTIFICATE").ok_or_else(|| CertError::Unreadable {
            path: cert_path.to_path_buf(),
            reason: "no CERTIFICATE block found".to_owned(),
        })?;
        let key_der = pem_to_der(&key_pem, "PRIVATE KEY").ok_or_else(|| CertError::Unreadable {
            path: key_path.to_path_buf(),
            reason: "no PRIVATE KEY block found".to_owned(),
        })?;

        let der = CertificateDer::from(der);
        let fingerprint = fingerprint_of(&der);
        Ok(Self {
            chain: vec![der],
            key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der)),
            fingerprint,
        })
    }

    /// The fingerprint as lowercase hex, for logs and for operators to publish.
    #[must_use]
    pub fn fingerprint_hex(&self) -> String {
        let mut out = String::with_capacity(64);
        for byte in &self.fingerprint {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
        }
        out
    }
}

/// `BLAKE3` of a DER certificate.
///
/// Domain-separated, so this hash can never collide with a hash of the same
/// bytes computed for another purpose.
#[must_use]
pub fn fingerprint_of(der: &CertificateDer<'_>) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"tiamot:server-cert:v1");
    hasher.update(der.as_ref());
    *hasher.finalize().as_bytes()
}

/// Extracts the first PEM block of a given label as DER bytes.
///
/// Hand-rolled rather than pulling in a PEM crate for forty lines. Only ever
/// reads files this server wrote, so it is strict and returns `None` on
/// anything unexpected rather than trying to recover.
fn pem_to_der(pem: &str, label: &str) -> Option<Vec<u8>> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let start = pem.find(&begin)? + begin.len();
    let stop = pem[start..].find(&end)? + start;
    let body: String = pem[start..stop].split_whitespace().collect();
    base64_decode(&body)
}

/// Minimal standard-alphabet base64 decoder.
fn base64_decode(text: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut accumulator: u32 = 0;
    let mut bits = 0u32;

    for byte in text.bytes() {
        if byte == b'=' {
            break;
        }
        let value = ALPHABET.iter().position(|c| *c == byte)?;
        // `value` is an index into a 64-entry table, so it fits in 6 bits.
        accumulator = (accumulator << 6) | u32::try_from(value).ok()?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((accumulator >> bits) & 0xFF).ok()?);
        }
    }
    Some(out)
}

fn read(path: &Path) -> Result<String, CertError> {
    std::fs::read_to_string(path).map_err(|source| CertError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), CertError> {
    std::fs::write(path, bytes).map_err(|source| CertError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<(), CertError> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|source| {
        CertError::Io {
            path: path.to_path_buf(),
            source,
        }
    })
}

#[cfg(not(unix))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "matches the Unix signature; Windows inherits directory ACLs and has no chmod \
              equivalent worth emulating here"
)]
fn restrict_permissions(_path: &Path) -> Result<(), CertError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("tiamot-cert-tests").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn a_first_run_generates_a_certificate() {
        let dir = scratch("first-run");
        let cert = ServerCert::load_or_create(&dir).expect("generate");

        assert_eq!(cert.chain.len(), 1);
        assert!(dir.join(KEY_FILE).exists());
        assert!(dir.join(CERT_FILE).exists());
        assert_eq!(cert.fingerprint_hex().len(), 64);
    }

    #[test]
    fn the_fingerprint_is_stable_across_restarts() {
        // The TOFU contract. If this changed on restart, every returning client
        // would see what looks exactly like a man-in-the-middle attack.
        let dir = scratch("stable");
        let first = ServerCert::load_or_create(&dir).expect("generate");
        let second = ServerCert::load_or_create(&dir).expect("reload");

        assert_eq!(
            first.fingerprint, second.fingerprint,
            "a restart must not change the fingerprint"
        );
        assert_eq!(
            first.chain[0], second.chain[0],
            "the DER must round-trip exactly"
        );
    }

    #[test]
    fn different_servers_get_different_fingerprints() {
        let a = ServerCert::load_or_create(&scratch("server-a")).expect("generate");
        let b = ServerCert::load_or_create(&scratch("server-b")).expect("generate");
        assert_ne!(
            a.fingerprint, b.fingerprint,
            "two servers must be distinguishable, or pinning is meaningless"
        );
    }

    #[test]
    fn the_key_file_is_not_world_readable() {
        let dir = scratch("perms");
        ServerCert::load_or_create(&dir).expect("generate");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dir.join(KEY_FILE))
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o077,
                0,
                "the private key must not be group/world readable"
            );
        }
    }

    #[test]
    fn a_corrupt_certificate_is_an_error_not_a_silent_regeneration() {
        // Silently regenerating would change the fingerprint and lock out every
        // client, with nothing in the log to explain it.
        let dir = scratch("corrupt");
        ServerCert::load_or_create(&dir).expect("generate");
        std::fs::write(dir.join(CERT_FILE), "not a certificate").expect("corrupt it");

        let err = ServerCert::load_or_create(&dir).expect_err("should refuse");
        assert!(
            err.to_string().contains("re-pin"),
            "the message must warn about the consequence: {err}"
        );
    }

    #[test]
    fn base64_round_trips_the_alphabet() {
        // The decoder is hand-rolled, so it gets its own test rather than
        // being trusted because the certificate happened to load.
        for (encoded, expected) in [
            ("", &[][..]),
            ("QQ==", b"A"),
            ("QUI=", b"AB"),
            ("QUJD", b"ABC"),
            ("QUJDRA==", b"ABCD"),
            ("+/8=", &[0xFB, 0xFF]),
        ] {
            assert_eq!(
                base64_decode(encoded).as_deref(),
                Some(expected),
                "decoding `{encoded}`"
            );
        }
        assert_eq!(base64_decode("!!!!"), None, "invalid input must not decode");
    }

    #[test]
    fn the_fingerprint_is_domain_separated() {
        // A bare BLAKE3 of the DER could collide with a hash of the same bytes
        // taken for another purpose — content addressing, for instance.
        let dir = scratch("domain");
        let cert = ServerCert::load_or_create(&dir).expect("generate");
        let bare = blake3::hash(cert.chain[0].as_ref());
        assert_ne!(
            cert.fingerprint,
            *bare.as_bytes(),
            "the fingerprint must be domain-separated from a bare hash"
        );
    }
}
