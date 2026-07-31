// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! A minimal protocol client.
//!
//! Connects, completes the join flow, and records everything the server sent.
//! Deliberately thin: a bot that hid the handshake behind a `connect()` helper
//! could not be used to test the handshake, and the identity suite needs to
//! drive it step by step — replaying a stale signature, signing for the wrong
//! server, presenting someone else's name.

use std::net::SocketAddr;
use std::sync::Arc;

use quinn::{ClientConfig, Endpoint};
use rustls_pki_types::CertificateDer;
use tiamot_core::identity::{Identity, challenge_payload};
use tiamot_core::proto::{
    ClientMessage, DisconnectReason, PROTOCOL_VERSION, ServerMessage, WireSignature,
};
use tiamot_server::transport::frame;

/// The ALPN the server requires. Must match `transport::endpoint`.
const ALPN: &[u8] = b"tiamot/1";

/// Something went wrong driving a connection.
#[derive(Debug, thiserror::Error)]
pub enum BotError {
    /// The QUIC endpoint could not be created or the connection failed.
    #[error("connection to {addr} failed: {reason}")]
    Connect {
        /// Where we tried to connect.
        addr: SocketAddr,
        /// Why it failed.
        reason: String,
    },

    /// A framed read or write failed.
    #[error(transparent)]
    Frame(#[from] frame::FrameError),

    /// The server sent something the flow did not expect.
    #[error("expected {expected} from the server, got {got}")]
    Unexpected {
        /// What the flow was waiting for.
        expected: &'static str,
        /// What arrived instead.
        got: String,
    },

    /// The server refused the connection.
    #[error("server refused the connection: {reason:?}")]
    Refused {
        /// Why, as the server put it.
        reason: DisconnectReason,
    },

    /// The server's certificate did not match the expected fingerprint.
    #[error(
        "server certificate fingerprint mismatch: expected {expected}, got {actual}. Under \
         trust-on-first-use this is either a different server or an interception."
    )]
    Fingerprint {
        /// What we pinned.
        expected: String,
        /// What the server presented.
        actual: String,
    },
}

/// A connected bot.
pub struct Bot {
    /// The identity this bot authenticates as.
    identity: Identity,
    endpoint: Endpoint,
    connection: quinn::Connection,
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    /// Everything the server has sent, in order.
    received: Vec<ServerMessage>,
    /// The fingerprint the server actually presented.
    cert_fingerprint: [u8; 32],
}

impl Bot {
    /// Connects to a server, verifying its certificate fingerprint.
    ///
    /// Does **not** start the join flow — the identity suite needs to drive
    /// that message by message.
    ///
    /// # Errors
    ///
    /// [`BotError::Connect`] if the transport fails, [`BotError::Fingerprint`]
    /// if the server presented a certificate other than the expected one.
    pub async fn connect(
        addr: SocketAddr,
        identity: Identity,
        expected_fingerprint: [u8; 32],
    ) -> Result<Self, BotError> {
        let mut endpoint =
            Endpoint::client("127.0.0.1:0".parse().map_err(|_| BotError::Connect {
                addr,
                reason: "could not parse the client bind address".to_owned(),
            })?)
            .map_err(|err| BotError::Connect {
                addr,
                reason: err.to_string(),
            })?;
        endpoint.set_default_client_config(client_config(expected_fingerprint));

        let connection = endpoint
            .connect(addr, "tiamot-server")
            .map_err(|err| BotError::Connect {
                addr,
                reason: err.to_string(),
            })?
            .await
            .map_err(|err| BotError::Connect {
                addr,
                reason: err.to_string(),
            })?;

        // The verifier already refused a mismatch during the handshake, so
        // reaching here means the fingerprint matched. Recomputing it gives the
        // bot the value to sign over without trusting what it was told.
        let cert_fingerprint = connection
            .peer_identity()
            .and_then(|any| any.downcast::<Vec<CertificateDer<'static>>>().ok())
            .and_then(|chain| chain.first().map(tiamot_server::cert::fingerprint_of))
            .unwrap_or(expected_fingerprint);

        let (send, recv) = connection
            .open_bi()
            .await
            .map_err(|err| BotError::Connect {
                addr,
                reason: err.to_string(),
            })?;

        Ok(Self {
            identity,
            endpoint,
            connection,
            send,
            recv,
            received: Vec::new(),
            cert_fingerprint,
        })
    }

    /// The fingerprint the server presented.
    #[must_use]
    pub const fn cert_fingerprint(&self) -> [u8; 32] {
        self.cert_fingerprint
    }

    /// This bot's identity.
    #[must_use]
    pub const fn identity(&self) -> &Identity {
        &self.identity
    }

    /// Everything the server has sent, in order.
    #[must_use]
    pub fn received(&self) -> &[ServerMessage] {
        &self.received
    }

    /// Sends one message.
    ///
    /// # Errors
    ///
    /// [`BotError::Frame`] if the write fails.
    pub async fn send(&mut self, message: &ClientMessage) -> Result<(), BotError> {
        frame::write(&mut self.send, message).await?;
        Ok(())
    }

    /// Reads one message, recording it.
    ///
    /// # Errors
    ///
    /// [`BotError::Frame`] if the read fails.
    pub async fn recv(&mut self) -> Result<ServerMessage, BotError> {
        let message: ServerMessage = frame::read(&mut self.recv).await?;
        self.received.push(message.clone());
        Ok(message)
    }

    /// Reads until a message matching `want` arrives, or the server disconnects.
    ///
    /// # Errors
    ///
    /// [`BotError::Refused`] if the server disconnected first,
    /// [`BotError::Frame`] on a transport failure.
    pub async fn recv_until(
        &mut self,
        want: fn(&ServerMessage) -> bool,
    ) -> Result<ServerMessage, BotError> {
        loop {
            let message = self.recv().await?;
            if want(&message) {
                return Ok(message);
            }
            if let ServerMessage::Disconnect { reason } = message {
                return Err(BotError::Refused { reason });
            }
        }
    }

    /// Sends `Hello` and returns the challenge nonce.
    ///
    /// # Errors
    ///
    /// [`BotError::Refused`] if the server rejected the hello.
    pub async fn hello(&mut self, display_name: &str) -> Result<[u8; 32], BotError> {
        self.send(&ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            public_key: *self.identity.public_key().as_bytes(),
            display_name: display_name.to_owned(),
        })
        .await?;

        let message = self
            .recv_until(|m| matches!(m, ServerMessage::AuthChallenge { .. }))
            .await?;
        match message {
            ServerMessage::AuthChallenge { nonce } => Ok(nonce),
            other => Err(BotError::Unexpected {
                expected: "AuthChallenge",
                got: format!("{other:?}"),
            }),
        }
    }

    /// Signs a challenge and sends the response.
    ///
    /// `fingerprint` is a parameter rather than taken from the connection so a
    /// test can deliberately sign for the wrong server — which is one of the
    /// identity suite's cases, and would be untestable if this helper always
    /// did the right thing.
    ///
    /// # Errors
    ///
    /// [`BotError::Frame`] if the write fails.
    pub async fn authenticate_with(
        &mut self,
        nonce: &[u8; 32],
        fingerprint: &[u8],
    ) -> Result<(), BotError> {
        let signature =
            self.identity
                .sign(&challenge_payload(nonce, fingerprint, PROTOCOL_VERSION));
        self.send(&ClientMessage::AuthResponse {
            signature: WireSignature(signature.to_bytes()),
        })
        .await
    }

    /// Signs the challenge correctly and sends the response.
    ///
    /// # Errors
    ///
    /// [`BotError::Frame`] if the write fails.
    pub async fn authenticate(&mut self, nonce: &[u8; 32]) -> Result<(), BotError> {
        let fingerprint = self.cert_fingerprint;
        self.authenticate_with(nonce, &fingerprint).await
    }

    /// Runs the whole join flow: hello, authenticate, join.
    ///
    /// # Errors
    ///
    /// [`BotError::Refused`] if the server disconnected at any point.
    pub async fn join(&mut self, display_name: &str) -> Result<(), BotError> {
        let nonce = self.hello(display_name).await?;
        self.authenticate(&nonce).await?;
        self.recv_until(|m| matches!(m, ServerMessage::ModManifest { .. }))
            .await?;
        self.send(&ClientMessage::JoinWorld).await?;
        self.recv_until(|m| matches!(m, ServerMessage::JoinWorld { .. }))
            .await?;
        Ok(())
    }

    /// Sends a block or sub-node edit.
    ///
    /// # Errors
    ///
    /// [`BotError::Frame`] if the write fails.
    pub async fn edit(&mut self, edit: tiamot_core::proto::Edit) -> Result<(), BotError> {
        self.send(&ClientMessage::BlockDelta { edit }).await
    }

    /// Waits for a `BlockDelta` to arrive, or gives up after `attempts` reads.
    ///
    /// Returns `None` on timeout rather than an error: "nothing arrived" is a
    /// legitimate outcome a test may be asserting, and forcing it through an
    /// error type would make the negative case read like a failure.
    ///
    /// # Errors
    ///
    /// [`BotError`] if the connection fails while waiting.
    pub async fn next_block_delta(
        &mut self,
        timeout: std::time::Duration,
    ) -> Result<Option<tiamot_core::proto::Edit>, BotError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            match tokio::time::timeout(remaining, self.recv()).await {
                Ok(Ok(ServerMessage::BlockDelta { edit, .. })) => return Ok(Some(edit)),
                Ok(Ok(_)) => {}
                Ok(Err(err)) => return Err(err),
                Err(_elapsed) => return Ok(None),
            }
        }
    }

    /// Collects chunks until `count` have arrived or the timeout expires.
    ///
    /// Returns them in arrival order, which is what a test asserting
    /// "nearest first" needs.
    ///
    /// # Errors
    ///
    /// [`BotError`] if the connection fails while waiting.
    pub async fn collect_chunks(
        &mut self,
        count: usize,
        timeout: std::time::Duration,
    ) -> Result<Vec<tiamot_core::ChunkPos>, BotError> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut chunks = Vec::new();
        while chunks.len() < count {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, self.recv()).await {
                Ok(Ok(ServerMessage::ChunkData { pos, .. })) => chunks.push(pos),
                Ok(Ok(_)) => {}
                Ok(Err(err)) => return Err(err),
                Err(_elapsed) => break,
            }
        }
        Ok(chunks)
    }

    /// Every chunk received so far, in arrival order.
    #[must_use]
    pub fn chunks_received(&self) -> Vec<tiamot_core::ChunkPos> {
        self.received
            .iter()
            .filter_map(|message| match message {
                ServerMessage::ChunkData { pos, .. } => Some(*pos),
                _ => None,
            })
            .collect()
    }

    /// Decodes a received chunk blob.
    ///
    /// The blob is the **same** format the world stores, so a client that can
    /// decode this can read a world file — which is the point: one format,
    /// exercised by both paths.
    ///
    /// # Errors
    ///
    /// [`BotError::Unexpected`] if no such chunk was received or it does not
    /// decode.
    pub fn decode_chunk(
        &self,
        pos: tiamot_core::ChunkPos,
        materials: &tiamot_core::persist::idmap::MaterialMap,
    ) -> Result<tiamot_core::Chunk, BotError> {
        let blob = self
            .received
            .iter()
            .rev()
            .find_map(|message| match message {
                ServerMessage::ChunkData { pos: got, blob } if *got == pos => Some(blob),
                _ => None,
            })
            .ok_or_else(|| BotError::Unexpected {
                expected: "a ChunkData for the requested position",
                got: format!("no chunk at {pos:?}"),
            })?;

        tiamot_core::persist::codec::decode_chunk(pos, blob, materials, &[]).map_err(|err| {
            BotError::Unexpected {
                expected: "a decodable chunk blob",
                got: err.to_string(),
            }
        })
    }

    /// Asks the server for content by hash.
    ///
    /// # Errors
    ///
    /// [`BotError::Frame`] if the write fails.
    pub async fn request_content(
        &mut self,
        hashes: Vec<tiamot_core::proto::ContentHash>,
    ) -> Result<(), BotError> {
        self.send(&ClientMessage::ContentRequest { hashes }).await
    }

    /// Collects and reassembles content until `wanted` items are complete.
    ///
    /// Verifies each item's hash against the bytes received. A server that sent
    /// something other than what was asked for is not trusted to say so
    /// itself — charter rule 14: server-pushed assets are hostile input, and
    /// the hash is the only part of the claim a client can check.
    ///
    /// # Errors
    ///
    /// [`BotError::Unexpected`] if a reassembled item does not match its hash.
    pub async fn collect_content(
        &mut self,
        wanted: usize,
        timeout: std::time::Duration,
    ) -> Result<Vec<(tiamot_core::proto::ContentHash, Vec<u8>)>, BotError> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut partial: std::collections::BTreeMap<tiamot_core::proto::ContentHash, Vec<u8>> =
            std::collections::BTreeMap::new();
        let mut complete = Vec::new();

        while complete.len() < wanted {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let message = match tokio::time::timeout(remaining, self.recv()).await {
                Ok(Ok(message)) => message,
                Ok(Err(err)) => return Err(err),
                Err(_elapsed) => break,
            };
            let ServerMessage::ContentChunk {
                hash,
                offset,
                total_len,
                data,
            } = message
            else {
                continue;
            };

            let plain = zstd::decode_all(data.as_slice()).map_err(|err| BotError::Unexpected {
                expected: "a zstd-compressed content slice",
                got: err.to_string(),
            })?;

            let buffer = partial.entry(hash).or_default();
            if buffer.len() as u64 != offset {
                return Err(BotError::Unexpected {
                    expected: "content slices in order",
                    got: format!("slice at {offset} but {} bytes held", buffer.len()),
                });
            }
            buffer.extend_from_slice(&plain);

            if buffer.len() as u64 >= total_len {
                let bytes = partial.remove(&hash).unwrap_or_default();
                // The hash is the whole point. A server that sent different
                // bytes than were asked for is caught here rather than by the
                // decoder it was aimed at.
                if tiamot_core::content::hash_bytes(&bytes) != hash {
                    return Err(BotError::Unexpected {
                        expected: "content matching the hash it was requested by",
                        got: "bytes that hash to something else".to_owned(),
                    });
                }
                complete.push((hash, bytes));
            }
        }
        Ok(complete)
    }

    /// The mod manifest the server sent, if it has arrived.
    #[must_use]
    pub fn manifest(&self) -> Option<&[tiamot_core::proto::ModEntry]> {
        self.received.iter().find_map(|message| match message {
            ServerMessage::ModManifest { mods, .. } => Some(mods.as_slice()),
            _ => None,
        })
    }

    /// Closes the connection cleanly.
    pub async fn disconnect(mut self) {
        let _ = self.send(&ClientMessage::Disconnect).await;
        let _ = self.send.finish();
        self.connection.close(0u32.into(), b"bye");
        self.endpoint.wait_idle().await;
    }

    /// Drops the connection without saying goodbye.
    ///
    /// For the "abrupt disconnect does not leak the player" test: a real client
    /// that loses power does not get to send a `Disconnect`.
    pub fn abandon(self) {
        // Dropping the endpoint tears the socket down without a close frame.
        drop(self);
    }
}

/// A quinn client config that pins one certificate fingerprint.
fn client_config(expected: [u8; 32]) -> ClientConfig {
    let mut tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .expect("TLS 1.3 is supported by the ring provider")
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(PinnedFingerprint { expected }))
    .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];

    let tls = quinn::crypto::rustls::QuicClientConfig::try_from(tls)
        .expect("a TLS 1.3 client config is a valid QUIC client config");
    ClientConfig::new(Arc::new(tls))
}

/// Accepts exactly one certificate: the one whose fingerprint was pinned.
///
/// This replaces CA validation rather than relaxing it. A verifier that
/// accepted anything would make the transport untested — the bot would connect
/// to a man-in-the-middle just as happily as to the real server, and the
/// fingerprint binding in the auth handshake would have nothing behind it.
#[derive(Debug)]
struct PinnedFingerprint {
    expected: [u8; 32],
}

impl rustls::client::danger::ServerCertVerifier for PinnedFingerprint {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls_pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls_pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let actual = tiamot_server::cert::fingerprint_of(end_entity);
        // Constant-time comparison is not required — the expected value is
        // public, and an attacker learning it by timing has learned nothing
        // they could not read from the server's own logs.
        if actual == self.expected {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "server certificate fingerprint does not match the pinned value".to_owned(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        // TLS 1.2 is not offered; reaching here would mean the version
        // restriction above was removed.
        Err(rustls::Error::General(
            "TLS 1.2 is not supported by this engine".to_owned(),
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        // The signature still has to be valid for the presented certificate.
        // Pinning replaces "who vouches for this key", not "does this peer hold
        // the key" — skipping this would let anyone replay a copied
        // certificate.
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
