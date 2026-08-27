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

/// A reported position in whole blocks, as `f64`.
///
/// `f64` because a chunk origin times 48 plus an offset is exactly the sum
/// charter rule 7 keeps out of `f32` — and this is presentation for a bot's
/// navigation rather than simulation, so the wider type costs nothing.
fn world_blocks(position: &PlayerPosition) -> [f64; 3] {
    let span = f64::from(tiamot_core::CHUNK_SUBNODES);
    let per_block = f64::from(tiamot_core::SUBNODES_PER_AXIS);
    [
        (f64::from(position.chunk.x) * span + f64::from(position.local[0])) / per_block,
        (f64::from(position.chunk.y) * span + f64::from(position.local[1])) / per_block,
        (f64::from(position.chunk.z) * span + f64::from(position.local[2])) / per_block,
    ]
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
    /// Which way this bot is looking, as `[yaw, pitch]` in turns.
    ///
    /// **Held rather than passed to every call**, because a real player's head
    /// stays where they left it: a test that had to name the direction on each
    /// step would be a test whose bot faced north whenever somebody forgot.
    look: [f32; 2],
}

impl Bot {
    /// Points this bot's head, as `[yaw, pitch]` in turns.
    ///
    /// Applies to every input sent afterwards, exactly as a mouse does. Yaw
    /// zero is north; a quarter turn is east.
    pub const fn look_at(&mut self, look: [f32; 2]) {
        self.look = look;
    }

    /// Which way this bot is looking.
    #[must_use]
    pub const fn looking(&self) -> [f32; 2] {
        self.look
    }
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
            // North, which is where a client starts. `look_at` moves it.
            look: [0.0, 0.0],
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

    /// Sends a retired `BlockDelta`, which the server now refuses.
    ///
    /// **Kept solely to prove the refusal.** Task 07 used this to write blocks
    /// straight into the world; Task 09 replaced it with digging and placing,
    /// and a message that skipped both made every rule they enforce optional.
    /// The variant stays on the wire because postcard encodes variants by
    /// ordinal and removing one renumbers everything after it — deprecated in
    /// place is the only way to retire one.
    ///
    /// # Errors
    ///
    /// [`BotError::Frame`] if the write fails. The DISCONNECT that follows is
    /// what a caller should be looking for; see [`Bot::refusal`].
    pub async fn send_retired_block_delta(
        &mut self,
        edit: tiamot_core::proto::Edit,
    ) -> Result<(), BotError> {
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

    /// The tools the server said its mods registered.
    ///
    /// Empty until the table arrives, and on a server whose mods registered
    /// none — which is a world nobody can dig in, and correct rather than
    /// broken (charter rule 1).
    #[must_use]
    pub fn tools(&self) -> Vec<tiamot_core::proto::ToolDef> {
        self.received()
            .into_iter()
            .find_map(|message| match message {
                ServerMessage::ToolTable { tools } => Some(tools),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// Holds the first tool with the given brush, if the mods registered one.
    ///
    /// Chosen by BRUSH rather than by name because the engine has no opinion
    /// about what a chisel is called — a bot that named one would be coupled to
    /// `game/` rather than to the API.
    ///
    /// # Errors
    ///
    /// [`BotError::Unexpected`] if no registered tool has that brush.
    pub async fn hold_brush(&mut self, brush: &str) -> Result<(), BotError> {
        let id = self
            .tools()
            .into_iter()
            .find(|tool| tool.brush == brush)
            .map(|tool| tool.id)
            .ok_or_else(|| BotError::Unexpected {
                expected: "a registered tool with the requested brush",
                got: format!("no tool has brush `{brush}`; the mod set registers none"),
            })?;
        self.select_tool(Some(&id)).await
    }

    /// Digs a whole block and waits for the server to confirm it is gone.
    ///
    /// **A real dig**, not a world edit: it holds a mod-registered tool with a
    /// whole-block brush, aims at the block's centre cell, and lets the server
    /// count the ticks. Until Task 09 this wrote air straight into the world,
    /// which meant every scenario using it was exercising a client's ability to
    /// edit the world — a capability that no longer exists.
    ///
    /// Re-aimed until it lands. Re-aiming at the same cell keeps its progress,
    /// so repeating costs nothing and survives a message going missing.
    ///
    /// # Errors
    ///
    /// [`BotError::Unexpected`] if no whole-block tool is registered, or if the
    /// block never breaks.
    pub async fn dig_block(&mut self, pos: tiamot_core::BlockPos) -> Result<(), BotError> {
        self.hold_brush(tiamot_core::dig::Brush::Block.name())
            .await?;
        // **Finished means every sub-node has gone, not one `Edit::Block`.**
        // A block comes apart a cell at a time now, so digging never emits a
        // whole-block edit at all — this used to wait for one and timed out on
        // a block that had visibly finished breaking.
        self.dig_until_gone(
            tiamot_core::SubNodePos::new(pos.x * 3 + 1, pos.y * 3 + 1, pos.z * 3 + 1),
            |bot| bot.block_is_empty(pos),
        )
        .await
    }

    /// Whether every sub-node of a block has been removed, as far as this bot
    /// has been told.
    ///
    /// Counts the distinct `SubNode` edits it has seen, and still honours a
    /// whole-block edit — a mod's `set_block` can still send one.
    #[must_use]
    pub fn block_is_empty(&self, pos: tiamot_core::BlockPos) -> bool {
        let mut gone = std::collections::BTreeSet::new();
        for message in self.received() {
            let ServerMessage::BlockDelta { edit, .. } = message else {
                continue;
            };
            match edit {
                tiamot_core::proto::Edit::Block {
                    pos: got,
                    material: got_material,
                } if got == pos => {
                    if got_material == tiamot_core::MaterialId::AIR.0 {
                        return true;
                    }
                    // Filled back in. Anything counted before it is stale.
                    gone.clear();
                }
                tiamot_core::proto::Edit::SubNode {
                    pos: got,
                    material: got_material,
                } if got.block() == pos => {
                    if got_material == tiamot_core::MaterialId::AIR.0 {
                        gone.insert((got.x, got.y, got.z));
                    } else {
                        gone.remove(&(got.x, got.y, got.z));
                    }
                }
                _ => {}
            }
        }
        gone.len() >= tiamot_core::block::SUBNODES_PER_BLOCK
    }

    /// Hits an entity.
    ///
    /// Reach and existence are the server's to judge, so this always sends and
    /// never reports whether the hit landed — asking would mean inventing a
    /// reply message for something the engine deliberately has no opinion
    /// about. A scenario checks the CONSEQUENCE, which is whatever the mod that
    /// handled it did.
    ///
    /// # Errors
    ///
    /// [`BotError`] if the message cannot be sent.
    pub async fn punch(&mut self, entity: u64) -> Result<(), BotError> {
        self.send(&ClientMessage::Punch { entity }).await
    }

    /// Aims at a cell until `done`, re-sending the dig each round.
    async fn dig_until_gone(
        &mut self,
        target: tiamot_core::SubNodePos,
        done: impl Fn(&Self) -> bool,
    ) -> Result<(), BotError> {
        /// Long enough for the slowest tool the reference mods register, with
        /// room for a lost message on top.
        const PATIENCE: std::time::Duration = std::time::Duration::from_secs(30);
        /// How long to keep reading after the block has gone, so the credit
        /// that arrived with the last bite is in hand.
        const SETTLE: std::time::Duration = std::time::Duration::from_millis(300);

        let deadline = tokio::time::Instant::now() + PATIENCE;
        loop {
            self.start_dig(target).await?;
            let round = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
            while tokio::time::Instant::now() < round {
                if done(self) {
                    // **Drain what came with it.** A dig's last bite and the
                    // inventory update that pays for it are produced in the
                    // same tick, and the edit is broadcast first — so a caller
                    // that stopped reading the moment the block emptied would
                    // see twenty-five units of a twenty-seven unit block and
                    // report that digging loses material. Two tests did exactly
                    // that when a block started coming apart in pieces.
                    let settle = tokio::time::Instant::now() + SETTLE;
                    while tokio::time::Instant::now() < settle {
                        let _ =
                            tokio::time::timeout(std::time::Duration::from_millis(20), self.recv())
                                .await;
                    }
                    return Ok(());
                }
                let _ =
                    tokio::time::timeout(std::time::Duration::from_millis(100), self.recv()).await;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(BotError::Unexpected {
                    expected: "the dig to complete",
                    got: format!("{target:?} was still there after {PATIENCE:?}"),
                });
            }
        }
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
        self.walk_facing(direction, actions, ticks, self.look).await
    }

    /// The same, looking a particular way.
    ///
    /// `look` is `[yaw, pitch]` in TURNS, as the wire carries it.
    ///
    /// **Every bot sent `[0.0, 0.0]` until this existed**, which meant every
    /// body in every test faced north for ever — and yaw zero is the one value
    /// where a camera's angle and a figure's agree. That is why the mirrored
    /// body in `157f4a3` survived two fixes: nothing in the suite could turn
    /// anybody, so nothing could see it.
    ///
    /// # Errors
    ///
    /// [`BotError::Frame`] if a read or write fails.
    pub async fn walk_facing(
        &mut self,
        direction: [f32; 3],
        actions: u32,
        ticks: u64,
        look: [f32; 2],
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
                    look,
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
        self.hold_brush(tiamot_core::dig::Brush::SubNode.name())
            .await?;
        self.dig_until_gone(pos, move |bot| {
            bot.received().iter().any(|message| {
                matches!(
                    message,
                    ServerMessage::BlockDelta {
                        edit: tiamot_core::proto::Edit::SubNode { pos: at, material },
                        ..
                    } if *at == pos && *material == tiamot_core::MaterialId::AIR.0
                )
            })
        })
        .await
    }

    /// Places one sub-node: fills a single cell, the one aimed at.
    ///
    /// The mirror of [`Bot::dig_subnode`], and it holds a tool the same way —
    /// by brush rather than by name, because the engine has no opinion about
    /// what a chisel is called (charter rule 1). The brush decides what a
    /// placement writes as well as what a dig removes, so holding a sub-node
    /// tool is what makes this one cell rather than a whole block.
    ///
    /// Unlike [`Bot::place`] this does not retry. A sub-node placement into an
    /// occupied cell is refused, so a retry that arrived after a successful
    /// first attempt would be reported as a failure of the wrong thing — the
    /// trap [`Bot::place`]'s own comment describes, from the other side.
    ///
    /// # Errors
    ///
    /// [`BotError::Unexpected`] if no sub-node tool is registered, or if the
    /// cell never fills. Read [`Bot::notices`] for the server's reason.
    pub async fn place_subnode(
        &mut self,
        pos: tiamot_core::SubNodePos,
        material: u16,
    ) -> Result<(), BotError> {
        /// Long enough to cover a tick and the broadcast back.
        const PATIENCE: std::time::Duration = std::time::Duration::from_secs(10);

        self.hold_brush(tiamot_core::dig::Brush::SubNode.name())
            .await?;
        self.place_from_inventory(pos, material).await?;

        let deadline = tokio::time::Instant::now() + PATIENCE;
        loop {
            if self.saw_subnode(pos, material) {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(BotError::Unexpected {
                    expected: "the placed cell to appear",
                    got: format!("nothing at {pos:?}; notices: {:?}", self.notices()),
                });
            }
            let _ = tokio::time::timeout(
                remaining.min(std::time::Duration::from_millis(100)),
                self.recv(),
            )
            .await;
        }
    }

    /// The light the server has most recently reported for a chunk.
    ///
    /// `None` until a `ChunkLight` arrives for it. Decoded here rather than
    /// stored raw so a scenario asserting about light does not have to know the
    /// wire format — and so a payload that will not decode fails the test that
    /// cares rather than being silently treated as darkness.
    #[must_use]
    pub fn light_at(&self, pos: tiamot_core::BlockPos) -> Option<tiamot_core::light::Light> {
        let chunk = pos.chunk();
        // The LAST one wins: light is re-sent whenever it changes, and an
        // earlier payload describes a world that has moved on.
        self.received()
            .iter()
            .rev()
            .find_map(|message| match message {
                ServerMessage::ChunkLight { pos: at, light } if *at == chunk => {
                    tiamot_core::light::codec::decode(light).ok()
                }
                _ => None,
            })
            .map(|layer| layer.get(pos.local()))
    }

    /// Every entity the server has told this bot about, by id.
    ///
    /// Rebuilt from the message history rather than kept as state, exactly as
    /// [`Bot::light_at`] is: what a bot knows is defined as what it was sent, so
    /// a scenario asserting about entities is asserting about the wire.
    ///
    /// Spawns insert, despawns remove, deltas move. A delta for an entity the
    /// bot never saw spawn is **ignored**, which is the honest reading: deltas
    /// go on the unreliable channel and spawns do not, so an unmatched delta is
    /// a stale packet about something already despawned, not a discovery.
    #[must_use]
    pub fn entities(&self) -> std::collections::BTreeMap<u64, tiamot_core::proto::EntityDef> {
        let mut live: std::collections::BTreeMap<u64, tiamot_core::proto::EntityDef> =
            std::collections::BTreeMap::new();
        for message in self.received().iter() {
            match message {
                ServerMessage::EntitySpawn { entities } => {
                    for entity in entities {
                        live.insert(entity.id, entity.clone());
                    }
                }
                ServerMessage::EntityDespawn { entities } => {
                    for id in entities {
                        live.remove(id);
                    }
                }
                ServerMessage::EntityState { entities, .. } => {
                    for delta in entities {
                        if let Some(known) = live.get_mut(&delta.id) {
                            known.chunk = delta.chunk;
                            known.local = delta.local;
                            known.velocity = delta.velocity;
                            known.yaw = delta.yaw;
                            known.pitch = delta.pitch;
                            known.anim = delta.anim;
                        }
                    }
                }
                ServerMessage::EntityArmed { entities } => {
                    for armed in entities {
                        if let Some(known) = live.get_mut(&armed.id) {
                            known.hands = armed.hands;
                        }
                    }
                }
                _ => {}
            }
        }
        live
    }

    /// Waits until the bot knows about an entity the filter accepts.
    ///
    /// The filter is a closure rather than a struct of optional fields because
    /// every scenario wants a different question — "the one called Alice", "any
    /// humanoid", "the one that follows people" — and a filter type would grow a
    /// field per test.
    ///
    /// # Errors
    ///
    /// [`BotError::Unexpected`] if the timeout expires with nothing matching.
    pub async fn expect_entity(
        &mut self,
        matches: impl Fn(&tiamot_core::proto::EntityDef) -> bool,
        timeout: std::time::Duration,
    ) -> Result<tiamot_core::proto::EntityDef, BotError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(found) = self.entities().into_values().find(|entity| matches(entity)) {
                return Ok(found);
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(BotError::Unexpected {
                    expected: "an entity matching the filter",
                    got: format!("{} entities, none of them it", self.entities().len()),
                });
            }
            match tokio::time::timeout(remaining, self.recv()).await {
                Ok(Ok(_)) => {}
                Ok(Err(err)) => return Err(err),
                Err(_) => {}
            }
        }
    }

    /// The fluid layer the server has most recently reported for a chunk.
    ///
    /// `None` until a `ChunkFluid` arrives for it. **The last one wins**, and
    /// that is the whole recovery story: every `ChunkFluid` is a keyframe rather
    /// than a delta, so a client that missed one is corrected by the next
    /// instead of drifting. A scenario asserting the client agrees with the
    /// server is asserting exactly that.
    ///
    /// Decoded here rather than stored raw, for the reason [`Bot::light_at`]
    /// gives: a payload that will not decode fails the test that cares instead
    /// of being silently treated as an empty pond.
    #[must_use]
    pub fn fluid_layer(
        &self,
        pos: tiamot_core::ChunkPos,
    ) -> Option<tiamot_core::fluid::FluidLayer> {
        self.received()
            .iter()
            .rev()
            .find_map(|message| match message {
                ServerMessage::ChunkFluid { pos: at, fluid } if *at == pos => {
                    tiamot_core::fluid::codec::decode(fluid).ok()
                }
                _ => None,
            })
    }

    /// What the server has most recently reported at one block.
    #[must_use]
    pub fn fluid_at(&self, pos: tiamot_core::BlockPos) -> tiamot_core::fluid::Fluid {
        self.fluid_layer(pos.chunk())
            .map_or(tiamot_core::fluid::Fluid::EMPTY, |layer| {
                layer.get(pos.local())
            })
    }

    /// Waits until the server reports light at `pos` satisfying `wanted`.
    ///
    /// # Errors
    ///
    /// [`BotError::Unexpected`] if the timeout expires first.
    pub async fn expect_light(
        &mut self,
        pos: tiamot_core::BlockPos,
        wanted: impl Fn(tiamot_core::light::Light) -> bool,
        timeout: std::time::Duration,
    ) -> Result<tiamot_core::light::Light, BotError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(level) = self.light_at(pos)
                && wanted(level)
            {
                return Ok(level);
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(BotError::Unexpected {
                    expected: "light matching the condition",
                    got: format!("{:?} at {pos:?}", self.light_at(pos)),
                });
            }
            let _ = tokio::time::timeout(
                remaining.min(std::time::Duration::from_millis(100)),
                self.recv(),
            )
            .await;
        }
    }

    /// Whether a cell edit setting `pos` to `material` has been broadcast.
    #[must_use]
    pub fn saw_subnode(&self, pos: tiamot_core::SubNodePos, material: u16) -> bool {
        self.received().iter().any(|message| {
            matches!(
                message,
                ServerMessage::BlockDelta {
                    edit: tiamot_core::proto::Edit::SubNode { pos: at, material: got },
                    ..
                } if *at == pos && *got == material
            )
        })
    }

    /// Places a material at a block position and waits for it to appear.
    ///
    /// **A real placement**, which means the player has to be CARRYING the
    /// material: this asks and the server decides. It used to write the block
    /// straight into the world, so a scenario could build out of nothing.
    ///
    /// # Errors
    ///
    /// [`BotError::Unexpected`] if the block never appears — which includes
    /// every reason the server may refuse. Read [`Bot::notices`] for which one.
    pub async fn place(
        &mut self,
        pos: tiamot_core::BlockPos,
        material: u16,
    ) -> Result<(), BotError> {
        /// Re-sent while waiting, because a request can go missing and a
        /// refusal is reported rather than retried.
        const PATIENCE: std::time::Duration = std::time::Duration::from_secs(10);

        // A whole block wants a whole-block brush, the same way [`Bot::dig_block`]
        // does — the brush decides what a placement writes. Without this, a
        // scenario that chiselled first would still be holding the chisel and
        // would place one cell, then wait for a block that was never coming.
        //
        // Only if the mod set has one, and no error if it does not: placing is
        // spending material already held rather than a rule a mod must supply,
        // so the server falls back to a block brush and a world with no tools
        // can still build. Digging cannot say the same, which is why
        // `dig_block` insists.
        if self
            .tools()
            .iter()
            .any(|tool| tool.brush == tiamot_core::dig::Brush::Block.name())
        {
            self.hold_brush(tiamot_core::dig::Brush::Block.name())
                .await?;
        }

        let target = tiamot_core::SubNodePos::new(pos.x * 3 + 1, pos.y * 3 + 1, pos.z * 3 + 1);
        let deadline = tokio::time::Instant::now() + PATIENCE;
        loop {
            self.place_from_inventory(target, material).await?;
            // **Either form counts.** 27 units is broadcast as `Edit::Block`
            // and anything less as `Edit::Partial`, and waiting only for the
            // first meant a spare-node placement never saw its own success —
            // so this retried, and the retries failed with "you are not
            // carrying any of that" because the FIRST attempt had worked and
            // spent everything. The symptom was a refusal that named the
            // opposite of the problem.
            let round = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
            while tokio::time::Instant::now() < round {
                if self.saw_block(pos, material) || self.saw_any_partial(pos, material) {
                    return Ok(());
                }
                let _ =
                    tokio::time::timeout(std::time::Duration::from_millis(100), self.recv()).await;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(BotError::Unexpected {
                    expected: "the placed block to appear",
                    got: format!("nothing at {pos:?}; notices: {:?}", self.notices()),
                });
            }
        }
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
    /// Presses or releases a mod-registered action, by id.
    ///
    /// **The same path a player's keyboard takes.** A client turns a key into
    /// an action id and sends that; this sends the id directly, which is what
    /// makes a bot scenario exercise the human path rather than a parallel one.
    /// Charter rule 11 is why it can: the id is the thing that travels, and no
    /// part of the server knows or cares which key produced it.
    ///
    /// Only actions the server registered are accepted — it refuses anything
    /// else at the edge — and the engine's own controls are not actions at all.
    ///
    /// # Errors
    ///
    /// [`BotError`] if the message cannot be sent.
    /// Every sound the server has told this bot to play, in arrival order.
    ///
    /// The whole of what Task 13's delivery criterion can assert: a bot has no
    /// speakers, and whether a noise came out is the [H] half. What IS testable
    /// is that the right players were told and the wrong ones were not.
    #[must_use]
    pub fn sounds_heard(&self) -> Vec<(String, [f64; 3])> {
        self.received()
            .into_iter()
            .filter_map(|message| match message {
                ServerMessage::PlaySound { sound, pos, .. } => Some((sound, pos)),
                _ => None,
            })
            .collect()
    }

    /// The sounds a server's mods registered.
    #[must_use]
    pub fn sound_table(&self) -> Option<Vec<tiamot_core::proto::SoundDef>> {
        self.received()
            .into_iter()
            .find_map(|message| match message {
                ServerMessage::SoundTable { sounds } => Some(sounds),
                _ => None,
            })
    }

    /// Every dialog the server has opened or replaced, in arrival order.
    #[must_use]
    pub fn dialogs(&self) -> Vec<(String, tiamot_core::ui::Tree)> {
        self.received()
            .into_iter()
            .filter_map(|message| match message {
                ServerMessage::ShowDialog { form, tree, .. }
                | ServerMessage::UpdateDialog { form, tree, .. } => Some((form, tree)),
                _ => None,
            })
            .collect()
    }

    /// Every dialog the server has closed.
    #[must_use]
    pub fn closed_dialogs(&self) -> Vec<String> {
        self.received()
            .into_iter()
            .filter_map(|message| match message {
                ServerMessage::CloseDialog { form } => Some(form),
                _ => None,
            })
            .collect()
    }

    /// What the server last said each inventory view holds.
    ///
    /// The LAST word per view, not every update: a test wants the current
    /// state, and the history is only noise.
    #[must_use]
    pub fn views(
        &self,
    ) -> std::collections::BTreeMap<String, Vec<Option<tiamot_core::proto::StackDef>>> {
        let mut latest = std::collections::BTreeMap::new();
        for message in self.received() {
            if let ServerMessage::ViewUpdate { view, slots, .. } = message {
                latest.insert(view, slots);
            }
        }
        latest
    }

    /// One view's slots, if the server has sent it.
    ///
    /// A convenience over [`Bot::views`], because a test asking about one view
    /// asks about one view.
    #[must_use]
    pub fn view(&self, name: &str) -> Option<Vec<Option<tiamot_core::proto::StackDef>>> {
        self.views().remove(name)
    }

    /// Says which hotbar slot is held, as a client does when a number key is
    /// pressed.
    ///
    /// # Errors
    ///
    /// [`BotError`] if the connection has gone.
    pub async fn select_slot(&mut self, slot: u16) -> Result<(), BotError> {
        self.send(&tiamot_core::proto::ClientMessage::SelectSlot { slot })
            .await
    }

    /// What the server last said is on this player's cursor.
    #[must_use]
    pub fn held(&self) -> Option<tiamot_core::proto::StackDef> {
        self.received()
            .into_iter()
            .filter_map(|message| match message {
                ServerMessage::ViewUpdate { held, .. } => Some(held),
                _ => None,
            })
            .next_back()
            .flatten()
    }

    /// Waits until a view satisfies `want`, and returns it.
    ///
    /// # Errors
    ///
    /// [`BotError::Unexpected`] if it never does within the patience.
    pub async fn until_view(
        &mut self,
        view: &str,
        want: impl Fn(&[Option<tiamot_core::proto::StackDef>]) -> bool,
    ) -> Result<Vec<Option<tiamot_core::proto::StackDef>>, BotError> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Some(slots) = self.views().get(view)
                && want(slots)
            {
                return Ok(slots.clone());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(BotError::Unexpected {
                    expected: "a view matching the condition",
                    got: format!("view `{view}` never satisfied it"),
                });
            }
            self.recv().await?;
        }
    }

    /// Sends a raw dialog event, exactly as a client would.
    ///
    /// **The forgery seam.** A test uses this to send an event for a form the
    /// server never opened, or a slot nobody owns — which is what a hostile
    /// client does, and what the server has to survive.
    pub async fn dialog_event(
        &mut self,
        form: &str,
        event: tiamot_core::proto::DialogEvent,
    ) -> Result<(), BotError> {
        self.send(&tiamot_core::proto::ClientMessage::DialogEvent {
            form: form.to_owned(),
            event,
        })
        .await
    }

    pub async fn action(&mut self, id: &str, pressed: bool) -> Result<(), BotError> {
        self.send(&tiamot_core::proto::ClientMessage::Action {
            id: id.to_owned(),
            pressed,
        })
        .await
    }

    pub async fn place_from_inventory(
        &mut self,
        target: tiamot_core::SubNodePos,
        material: u16,
    ) -> Result<(), BotError> {
        self.send(&tiamot_core::proto::ClientMessage::Place {
            target,
            material,
            // Loose material. A bot that wants to place a cut stack builds the
            // message itself; this helper is the ordinary case.
            shape: 0,
        })
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

    /// Whether any partial placement of this material has landed at `pos`.
    ///
    /// Unlike [`Bot::saw_partial`] this does not care how many cells were
    /// filled — for a caller that only needs to know the placement happened.
    fn saw_any_partial(&self, pos: tiamot_core::BlockPos, material: u16) -> bool {
        self.received().iter().any(|message| {
            matches!(
                message,
                ServerMessage::BlockDelta {
                    edit: tiamot_core::proto::Edit::Partial {
                        pos: got,
                        material: got_material,
                        ..
                    },
                    ..
                } if *got == pos && *got_material == material
            )
        })
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
    /// Whether a block delta for this position and material has arrived.
    ///
    /// Public so a test can assert a mark did NOT appear — the shape every
    /// "the server refused it" test needs, and one `expect_block` cannot give
    /// because waiting for something that should never happen has no deadline
    /// that means anything.
    #[must_use]
    pub fn saw_block(&self, pos: tiamot_core::BlockPos, material: u16) -> bool {
        // **Air arrives two ways now.** A dig takes a block apart one sub-node
        // at a time and never sends a whole-block edit, so "is it air yet" has
        // to count the pieces — while "is it stone yet" is still one edit,
        // because nothing builds a block up a cell at a time.
        if material == tiamot_core::MaterialId::AIR.0 && self.block_is_empty(pos) {
            return true;
        }
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
    pub fn inventory(&self) -> Vec<tiamot_core::proto::StackDef> {
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
    ) -> Result<Vec<tiamot_core::proto::StackDef>, BotError> {
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
            .filter(|stack| stack.material == material)
            .map(|stack| stack.units)
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
        /// How close, in blocks, counts as arrived.
        const ARRIVED: f64 = 1.0;
        /// Ticks of walking between course corrections.
        const LEG: u64 = 5;
        /// Legs before giving up. A straight line cannot get everywhere.
        const LEGS: u32 = 40;

        let _ = y;
        let mut stalled = 0u32;
        let mut previous: Option<[f64; 3]> = None;

        for _ in 0..LEGS {
            // Where the SERVER says we are. Walking toward a position the bot
            // merely believed would drift, and the drift compounds.
            let here = self.walk([0.0; 3], 0, 1).await?;
            let at = world_blocks(&here);
            let to = [f64::from(x) - at[0], f64::from(z) - at[2]];
            let distance = (to[0] * to[0] + to[1] * to[1]).sqrt();
            if distance <= ARRIVED {
                return Ok(());
            }

            // No progress since the last leg means something is in the way.
            // Jump at it: a straight line with a jump gets over the one-block
            // steps terrain actually has, and anything more is pathfinding,
            // which a load bot has no business doing.
            let actions = if previous.is_some_and(|was| {
                let moved = (at[0] - was[0]).abs() + (at[2] - was[2]).abs();
                moved < 0.1
            }) {
                stalled += 1;
                tiamot_core::proto::actions::JUMP
            } else {
                stalled = 0;
                0
            };
            if stalled > 4 {
                // Jumping is not helping either. Report where it got to rather
                // than spinning: a caller that needs to be somewhere exact will
                // notice, and one that just wanted to move has moved.
                return Ok(());
            }
            previous = Some(at);

            let direction = [(to[0] / distance) as f32, 0.0, (to[1] / distance) as f32];
            self.walk(direction, actions, LEG).await?;
        }
        Ok(())
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
