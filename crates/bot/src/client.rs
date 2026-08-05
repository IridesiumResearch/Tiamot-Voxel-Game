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
pub use tiamot_server::transport::{Impairment, Link};

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

/// Where the server says a player is.
///
/// Charter rule 7's pair, in the units the physics uses: a chunk, and an offset
/// inside it measured in sub-node cells.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlayerPosition {
    /// Which chunk.
    pub chunk: tiamot_core::ChunkPos,
    /// Cell offset within it, `0..48` on each axis.
    pub local: [f32; 3],
}

impl PlayerPosition {
    /// The position in whole blocks, for asserting about where a bot ended up.
    #[must_use]
    pub fn block(&self) -> tiamot_core::BlockPos {
        let corner = tiamot_core::BlockPos::from_chunk_corner(self.chunk);
        let cells = tiamot_core::SUBNODES_PER_AXIS as f32;
        tiamot_core::BlockPos::new(
            corner.x + tiamot_core::detgen::floor_to_i32(self.local[0] / cells),
            corner.y + tiamot_core::detgen::floor_to_i32(self.local[1] / cells),
            corner.z + tiamot_core::detgen::floor_to_i32(self.local[2] / cells),
        )
    }
}

/// A connected bot.
pub struct Bot {
    /// The identity this bot authenticates as.
    identity: Identity,
    endpoint: Endpoint,
    connection: quinn::Connection,
    /// The control stream, and the artificial conditions applied to it.
    send: Link,
    /// Messages the reader task has taken off the wire but the script has not
    /// consumed yet.
    inbox: tokio::sync::mpsc::UnboundedReceiver<ServerMessage>,
    /// Everything the server has sent, in order.
    ///
    /// Shared with the reader task, which appends as messages arrive — so
    /// `inventory()` and `expect_block` see a message the moment it lands
    /// rather than when the script next calls `recv`.
    history: Arc<std::sync::Mutex<Vec<ServerMessage>>>,
    /// The reader task, aborted when the bot goes away.
    reader: tokio::task::JoinHandle<()>,
    /// The fingerprint the server actually presented.
    cert_fingerprint: [u8; 32],
}

impl Drop for Bot {
    fn drop(&mut self) {
        // The reader borrows nothing from the bot, so it would otherwise
        // outlive it and hold the connection open. The delayed writer, which
        // owns the send stream when one is running, is cleaned up by `Link`.
        self.reader.abort();
    }
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
        Self::connect_with_verifier(
            addr,
            identity,
            Arc::new(PinnedFingerprint {
                expected: expected_fingerprint,
            }),
            expected_fingerprint,
        )
        .await
    }

    async fn connect_with_verifier(
        addr: SocketAddr,
        identity: Identity,
        verifier: Arc<dyn rustls::client::danger::ServerCertVerifier>,
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
        endpoint.set_default_client_config(client_config(verifier));

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

        let (send, mut recv) = connection
            .open_bi()
            .await
            .map_err(|err| BotError::Connect {
                addr,
                reason: err.to_string(),
            })?;

        // A dedicated reader, running whatever the script is doing.
        //
        // This is the fix for a class of bug rather than one instance of it: a
        // bot that only read when the script asked let the server's broadcast
        // back up until QUIC flow control stopped the server writing, at which
        // point the server stopped draining ITS side and both ends waited for
        // each other. Draining periodically from `send` only moved the
        // threshold — under parallel test load the server still outpaced it.
        //
        // Every real network client has a read loop independent of its
        // application logic. This is that.
        let history: Arc<std::sync::Mutex<Vec<ServerMessage>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let (inbox_tx, inbox) = tokio::sync::mpsc::unbounded_channel();
        let reader_history = Arc::clone(&history);
        let reader = tokio::spawn(async move {
            loop {
                match frame::read::<_, ServerMessage>(&mut recv).await {
                    Ok(message) => {
                        if let Ok(mut history) = reader_history.lock() {
                            history.push(message.clone());
                        }
                        if inbox_tx.send(message).is_err() {
                            return;
                        }
                    }
                    // The connection ended, cleanly or otherwise. The script
                    // finds out when its next read returns nothing.
                    Err(_) => return,
                }
            }
        });

        Ok(Self {
            identity,
            endpoint,
            connection,
            send: Link::new(send),
            inbox,
            history,
            reader,
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
    ///
    /// A snapshot: the reader task appends concurrently, so this is what had
    /// arrived when it was called.
    #[must_use]
    pub fn received(&self) -> Vec<ServerMessage> {
        self.history
            .lock()
            .map(|history| history.clone())
            .unwrap_or_default()
    }

    /// Sends one message.
    ///
    /// Drains anything already waiting first. A bot that only ever wrote would
    /// let the server's broadcast back up until QUIC flow control stopped the
    /// server writing — at which point the server stops draining ITS side and
    /// both ends stall until the connection times out.
    ///
    /// `churn.lua` hit exactly this: 160 edits with no reads. Linux socket
    /// buffers happened to absorb it and Windows CI did not, which is the worst
    /// way to find a bug of this shape.
    ///
    /// # Errors
    ///
    /// [`BotError::Frame`] if the write fails.
    pub async fn send(&mut self, message: &ClientMessage) -> Result<(), BotError> {
        self.send.write(message).await?;
        Ok(())
    }

    /// Applies artificial latency and loss to everything sent from now on.
    ///
    /// Outbound only. Impairing the inbound side would need the reader task to
    /// hold messages back, and the interesting direction is this one: what the
    /// engine has to survive is an input that arrives late or not at all, and
    /// that is decided on the way TO the server.
    pub fn impair(&mut self, impairment: Impairment) {
        self.send.impair(impairment);
    }

    /// The conditions currently being simulated.
    #[must_use]
    pub const fn impairment(&self) -> Impairment {
        self.send.impairment()
    }

    /// Takes the next message the reader task has queued.
    ///
    /// # Errors
    ///
    /// [`BotError::Frame`] if the connection has ended.
    pub async fn recv(&mut self) -> Result<ServerMessage, BotError> {
        self.inbox.recv().await.ok_or_else(|| {
            BotError::Frame(frame::FrameError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the connection ended",
            )))
        })
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
        self.received()
            .into_iter()
            .filter_map(|message| match message {
                ServerMessage::ChunkData { pos, .. } => Some(pos),
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
        let history = self.received();
        let blob = history
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

    /// The material table the server sent, if it has arrived.
    ///
    /// The ids in it are **world** ids — the ones chunk blobs carry — so this
    /// is what turns a decoded chunk's numbers into names a client can choose
    /// textures by.
    #[must_use]
    pub fn material_table(&self) -> Option<Vec<tiamot_core::proto::MaterialDef>> {
        self.received()
            .into_iter()
            .find_map(|message| match message {
                ServerMessage::MaterialTable { materials } => Some(materials),
                _ => None,
            })
    }

    /// The mod manifest the server sent, if it has arrived.
    #[must_use]
    pub fn manifest(&self) -> Option<Vec<tiamot_core::proto::ModEntry>> {
        self.received()
            .into_iter()
            .find_map(|message| match message {
                ServerMessage::ModManifest { mods, .. } => Some(mods),
                _ => None,
            })
    }

    /// Authorises another key for this bot's identity.
    ///
    /// Signed by this bot's key, which must already be authorised.
    ///
    /// # Errors
    ///
    /// [`BotError::Frame`] if the write fails.
    pub async fn add_key(
        &mut self,
        new_key: &ed25519_dalek::VerifyingKey,
        next_key_hash: Option<[u8; 32]>,
    ) -> Result<(), BotError> {
        let uuid = self.identity.uuid_as_root();
        let payload =
            tiamot_core::identity::keyset::add_key_payload(&uuid, new_key, next_key_hash.as_ref());
        self.send(&ClientMessage::AddKey {
            new_public_key: *new_key.as_bytes(),
            next_key_hash,
            signature: WireSignature(self.identity.sign(&payload).to_bytes()),
            signer_public_key: *self.identity.public_key().as_bytes(),
        })
        .await
    }

    /// Authorises another key, signing with a *different* identity.
    ///
    /// For the negative case: an addition signed by an unauthorised key must be
    /// refused. A helper that always signed correctly could not test that.
    ///
    /// # Errors
    ///
    /// [`BotError::Frame`] if the write fails.
    pub async fn add_key_signed_by(
        &mut self,
        signer: &Identity,
        target_uuid: &tiamot_core::PlayerUuid,
        new_key: &ed25519_dalek::VerifyingKey,
    ) -> Result<(), BotError> {
        let payload = tiamot_core::identity::keyset::add_key_payload(target_uuid, new_key, None);
        self.send(&ClientMessage::AddKey {
            new_public_key: *new_key.as_bytes(),
            next_key_hash: None,
            signature: WireSignature(signer.sign(&payload).to_bytes()),
            signer_public_key: *signer.public_key().as_bytes(),
        })
        .await
    }

    /// Rotates this bot's key to a successor.
    ///
    /// # Errors
    ///
    /// [`BotError::Frame`] if the write fails.
    pub async fn rotate_key(
        &mut self,
        new_key: &ed25519_dalek::VerifyingKey,
        new_next_key_hash: Option<[u8; 32]>,
    ) -> Result<(), BotError> {
        let uuid = self.identity.uuid_as_root();
        let payload = tiamot_core::identity::keyset::rotate_key_payload(
            &uuid,
            new_key,
            new_next_key_hash.as_ref(),
        );
        self.send(&ClientMessage::RotateKey {
            new_public_key: *new_key.as_bytes(),
            new_next_key_hash,
            signature: WireSignature(self.identity.sign(&payload).to_bytes()),
        })
        .await
    }

    /// Whether the server has disconnected, and why.
    ///
    /// Waits up to `timeout` for a `Disconnect`, SKIPPING anything else that
    /// arrives meanwhile. "Nothing arrived" means the operation was accepted,
    /// since the server answers a refusal and stays silent on success.
    ///
    /// Skipping is load-bearing rather than defensive. This used to read
    /// exactly one message and treat anything that was not a refusal as
    /// acceptance, which was true only while a joined-but-idle connection was
    /// silent. Since Task 09 the server sends a `PlayerState` every tick, so
    /// the first message after any request is almost always that — and every
    /// "was this refused?" test started reporting "accepted".
    ///
    /// # Errors
    ///
    /// [`BotError`] on a transport failure other than a timeout.
    pub async fn refusal(
        &mut self,
        timeout: std::time::Duration,
    ) -> Result<Option<DisconnectReason>, BotError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            match tokio::time::timeout(remaining, self.recv()).await {
                Ok(Ok(ServerMessage::Disconnect { reason })) => return Ok(Some(reason)),
                Ok(Ok(_)) => {}
                Ok(Err(_)) | Err(_) => return Ok(None),
            }
        }
    }

    /// Digs a whole block: replaces it with air.
    ///
    /// # Errors
    ///
    /// [`BotError::Frame`] if the write fails.
    pub async fn dig_block(&mut self, pos: tiamot_core::BlockPos) -> Result<(), BotError> {
        self.edit(tiamot_core::proto::Edit::Block {
            pos,
            material: tiamot_core::MaterialId::AIR.0,
        })
        .await
    }

    /// Walks in a world-space direction until the server has simulated
    /// `ticks` of it, and returns where the server says the player ended up.
    ///
    /// Drives from the server's own [`ServerMessage::PlayerState`] rather than
    /// from wall-clock time: it sends inputs for the ticks just after the one
    /// the server last processed, so the queue is always fed slightly ahead
    /// without ever running past the lookahead that would see them refused.
    /// Sleeping and hoping instead is the Task 07 bot bug — the one where a
    /// test passed on a fast machine and failed in CI.
    ///
    /// # Errors
    ///
    /// [`BotError::Frame`] if a read or write fails.
    pub async fn walk(
        &mut self,
        direction: [f32; 3],
        actions: u32,
        ticks: u64,
    ) -> Result<PlayerPosition, BotError> {
        /// How far ahead of the server to keep the queue fed. Comfortably
        /// inside `phys::input::MAX_LOOKAHEAD`, and more than a round trip.
        const AHEAD: u64 = 8;

        let mut started: Option<u64> = None;

        loop {
            let message = self.recv().await?;
            let ServerMessage::PlayerState {
                last_processed_input,
                chunk,
                local,
                ..
            } = message
            else {
                continue;
            };

            let start = *started.get_or_insert(last_processed_input);
            if last_processed_input >= start + ticks {
                return Ok(PlayerPosition { chunk, local });
            }

            for offset in 1..=AHEAD {
                self.send(&ClientMessage::PlayerInput {
                    tick: last_processed_input + offset,
                    movement: direction,
                    look: [0.0, 0.0],
                    actions,
                })
                .await?;
            }
        }
    }

    /// Asks the server to start breaking a cell.
    ///
    /// The server counts the ticks; this only says where to point. Sending it
    /// again with a different target re-aims and discards progress.
    ///
    /// # Errors
    ///
    /// [`BotError::Frame`] if the write fails.
    pub async fn start_dig(&mut self, target: tiamot_core::SubNodePos) -> Result<(), BotError> {
        self.send(&ClientMessage::StartDig { target }).await
    }

    /// Stops breaking, discarding progress.
    ///
    /// # Errors
    ///
    /// [`BotError::Frame`] if the write fails.
    pub async fn stop_dig(&mut self) -> Result<(), BotError> {
        self.send(&ClientMessage::CancelDig).await
    }

    /// Chooses the held tool. `None` is a bare hand.
    ///
    /// # Errors
    ///
    /// [`BotError::Frame`] if the write fails.
    pub async fn select_tool(&mut self, tool: Option<&str>) -> Result<(), BotError> {
        self.send(&ClientMessage::SelectTool {
            tool: tool.map(str::to_owned),
        })
        .await
    }

    /// Digs one sub-node: replaces a single cell with air.
    ///
    /// One of 27, which is the whole point of the engine. A client that could
    /// only dig whole blocks would make the sub-node design unreachable.
    ///
    /// # Errors
    ///
    /// [`BotError::Frame`] if the write fails.
    pub async fn dig_subnode(&mut self, pos: tiamot_core::SubNodePos) -> Result<(), BotError> {
        self.edit(tiamot_core::proto::Edit::SubNode {
            pos,
            material: tiamot_core::MaterialId::AIR.0,
        })
        .await
    }

    /// Places a material at a block position.
    ///
    /// # Errors
    ///
    /// [`BotError::Frame`] if the write fails.
    pub async fn place(
        &mut self,
        pos: tiamot_core::BlockPos,
        material: u16,
    ) -> Result<(), BotError> {
        self.edit(tiamot_core::proto::Edit::Block { pos, material })
            .await
    }

    /// Waits until the server confirms `pos` holds `material`.
    ///
    /// Watches the `BlockDelta` broadcast rather than asking, because there is
    /// no "read a block" message and there should not be one: a client that
    /// could query arbitrary world positions is a client that can map the
    /// server without walking it.
    ///
    /// # Errors
    ///
    /// [`BotError::Unexpected`] if the timeout expires without the edit
    /// arriving.
    pub async fn expect_block(
        &mut self,
        pos: tiamot_core::BlockPos,
        material: u16,
        timeout: std::time::Duration,
    ) -> Result<(), BotError> {
        let deadline = tokio::time::Instant::now() + timeout;
        // Anything already received counts: the confirmation may have arrived
        // while the caller was doing something else.
        if self.saw_block(pos, material) {
            return Ok(());
        }
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(BotError::Unexpected {
                    expected: "a block delta confirming the edit",
                    got: format!("nothing for {pos:?} = {material} within the timeout"),
                });
            }
            match tokio::time::timeout(remaining, self.recv()).await {
                Ok(Ok(_)) => {
                    if self.saw_block(pos, material) {
                        return Ok(());
                    }
                }
                Ok(Err(err)) => return Err(err),
                Err(_elapsed) => {
                    return Err(BotError::Unexpected {
                        expected: "a block delta confirming the edit",
                        got: format!("nothing for {pos:?} = {material} within the timeout"),
                    });
                }
            }
        }
    }

    /// Asks the server to place material from the inventory.
    ///
    /// The real placement path, unlike [`Bot::place`], which is the Task 07
    /// direct-edit stand-in and writes the world without paying for it. This
    /// sends a *request*: the server decides how much is actually placed from
    /// what the player is carrying, and may refuse it outright. Nothing comes
    /// back on success beyond the ordinary `BlockDelta` broadcast — use
    /// [`Bot::expect_partial`] to wait for it, and read [`Bot::notices`] for
    /// the reason if it never arrives.
    ///
    /// # Errors
    ///
    /// [`BotError::Frame`] if the write fails.
    pub async fn place_from_inventory(
        &mut self,
        target: tiamot_core::SubNodePos,
        material: u16,
    ) -> Result<(), BotError> {
        self.send(&tiamot_core::proto::ClientMessage::Place { target, material })
            .await
    }

    /// Waits until the server confirms a partially-filled block at `pos`.
    ///
    /// `cells` is how many sub-nodes are expected to be filled, which is what
    /// makes this an assertion about spare-node arithmetic rather than about
    /// something merely having happened.
    ///
    /// # Errors
    ///
    /// [`BotError::Unexpected`] if the timeout expires first.
    pub async fn expect_partial(
        &mut self,
        pos: tiamot_core::BlockPos,
        material: u16,
        cells: u32,
        timeout: std::time::Duration,
    ) -> Result<(), BotError> {
        let deadline = tokio::time::Instant::now() + timeout;
        if self.saw_partial(pos, material, cells) {
            return Ok(());
        }
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(BotError::Unexpected {
                    expected: "a block delta confirming a partial placement",
                    got: format!(
                        "nothing for {pos:?} = {material} with {cells} cells within the \
                         timeout; notices: {:?}",
                        self.notices()
                    ),
                });
            }
            match tokio::time::timeout(remaining, self.recv()).await {
                Ok(Ok(_)) => {
                    if self.saw_partial(pos, material, cells) {
                        return Ok(());
                    }
                }
                Ok(Err(err)) => return Err(err),
                Err(_) => {}
            }
        }
    }

    /// Whether a matching partial placement has been seen.
    fn saw_partial(&self, pos: tiamot_core::BlockPos, material: u16, cells: u32) -> bool {
        self.received().iter().rev().any(|message| {
            matches!(
                message,
                ServerMessage::BlockDelta {
                    edit: tiamot_core::proto::Edit::Partial {
                        pos: got,
                        material: got_material,
                        occupancy,
                    },
                    ..
                } if *got == pos
                    && *got_material == material
                    && occupancy.count_ones() == cells
            )
        })
    }

    /// Everything the server has said to this player alone.
    ///
    /// Chat with no sender: the server's answer when it refused to do
    /// something. Without reading these, a refused placement and a lost packet
    /// look identical from a script.
    #[must_use]
    pub fn notices(&self) -> Vec<String> {
        self.received()
            .iter()
            .filter_map(|message| match message {
                ServerMessage::Chat { from: None, text } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// Whether a matching block delta has been seen.
    fn saw_block(&self, pos: tiamot_core::BlockPos, material: u16) -> bool {
        self.received().iter().rev().any(|message| {
            matches!(
                message,
                ServerMessage::BlockDelta {
                    edit: tiamot_core::proto::Edit::Block { pos: got, material: got_material },
                    ..
                } if *got == pos && *got_material == material
            )
        })
    }

    /// The most recent inventory the server sent, in units.
    ///
    /// Empty until the server has sent one. Charter rule 5: these are **units**,
    /// so 27 is one block — use [`tiamot_core::inventory::display`] to split
    /// them into blocks and spare nodes.
    #[must_use]
    pub fn inventory(&self) -> Vec<(u16, u32)> {
        self.received()
            .iter()
            .rev()
            .find_map(|message| match message {
                ServerMessage::InventoryUpdate { stacks } => Some(stacks.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// Reads until an inventory update arrives, or the timeout expires.
    ///
    /// # Errors
    ///
    /// [`BotError`] on a transport failure.
    pub async fn await_inventory(
        &mut self,
        timeout: std::time::Duration,
    ) -> Result<Vec<(u16, u32)>, BotError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Ok(self.inventory());
            }
            match tokio::time::timeout(remaining, self.recv()).await {
                Ok(Ok(ServerMessage::InventoryUpdate { stacks })) => return Ok(stacks),
                Ok(Ok(_)) => {}
                Ok(Err(err)) => return Err(err),
                Err(_elapsed) => return Ok(self.inventory()),
            }
        }
    }

    /// Total units of one material currently held.
    #[must_use]
    pub fn units_of(&self, material: u16) -> u32 {
        self.inventory()
            .iter()
            .filter(|(id, _)| *id == material)
            .map(|(_, units)| *units)
            .sum()
    }

    /// Sends a chat line.
    ///
    /// # Errors
    ///
    /// [`BotError::Frame`] if the write fails.
    pub async fn chat(&mut self, text: &str) -> Result<(), BotError> {
        self.send(&ClientMessage::Chat {
            text: text.to_owned(),
        })
        .await
    }

    /// Sends a movement input toward a position.
    ///
    /// Teleport-shaped for now: there is no server-side physics until Task 09,
    /// so this reports intent and the server records it. The signature is the
    /// one real movement will use, so scripts written today keep working when
    /// the backend changes — which is the point of specifying it now.
    ///
    /// # Errors
    ///
    /// [`BotError::Frame`] if the write fails.
    pub async fn move_to(&mut self, x: f32, y: f32, z: f32) -> Result<(), BotError> {
        self.send(&ClientMessage::PlayerInput {
            tick: 0,
            movement: [x, y, z],
            look: [0.0, 0.0],
            actions: 0,
        })
        .await
    }

    /// Waits roughly `ticks` server ticks.
    pub async fn sleep_ticks(&mut self, ticks: u32) {
        tokio::time::sleep(tiamot_core::tick::TICK_DURATION * ticks).await;
    }

    /// Closes the connection cleanly.
    pub async fn disconnect(mut self) {
        let _ = self.send(&ClientMessage::Disconnect).await;
        self.send.finish();
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

impl Bot {
    /// Connects without knowing the fingerprint in advance, reporting what the
    /// server presented.
    ///
    /// This is **trust-on-first-use in its weakest form**: the first connection
    /// is trusted blindly and nothing is remembered afterwards. Fine for a
    /// command-line tool pointed at a server the operator chose, and wrong for
    /// anything that needs to notice an interception — which is why the tests
    /// use [`Bot::connect`] with an expected fingerprint instead.
    ///
    /// The fingerprint it saw is available from
    /// [`cert_fingerprint`](Bot::cert_fingerprint), so a caller can print it
    /// and pin it next time.
    ///
    /// # Errors
    ///
    /// [`BotError::Connect`] if the transport fails.
    pub async fn connect_trusting(addr: SocketAddr, identity: Identity) -> Result<Self, BotError> {
        Self::connect_with_verifier(addr, identity, Arc::new(AcceptAny), [0u8; 32]).await
    }
}

/// Accepts any certificate, recording nothing.
///
/// Used only by [`Bot::connect_trusting`]. Kept as a separate type rather than
/// a flag on the pinning verifier so that "accept anything" can never be the
/// result of a mis-set field.
#[derive(Debug)]
struct AcceptAny;

impl rustls::client::danger::ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls_pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls_pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
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
        // Still checked: the peer must hold the key for the certificate it
        // presented. Skipping this would let anyone replay a copied one.
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

/// A quinn client config that pins one certificate fingerprint.
fn client_config(verifier: Arc<dyn rustls::client::danger::ServerCertVerifier>) -> ClientConfig {
    let mut tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .expect("TLS 1.3 is supported by the ring provider")
    .dangerous()
    .with_custom_certificate_verifier(verifier)
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
