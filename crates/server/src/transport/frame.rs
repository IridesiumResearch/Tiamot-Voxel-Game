// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Length-prefixed message framing over a QUIC stream.
//!
//! QUIC streams are byte streams, not message streams: a `read` returns
//! whatever bytes have arrived, which may be half a message or three of them.
//! Something has to say where one message ends and the next begins, and that is
//! a 4-byte big-endian length prefix.
//!
//! # The length cap is checked before allocating
//!
//! Charter rule 14. A peer sends the length before the body, so a hostile peer
//! can claim four gigabytes and watch the server reserve it. The prefix is
//! therefore validated against [`MAX_MESSAGE_BYTES`] *before* any buffer is
//! sized from it — the check is on the claim, and the read that follows cannot
//! exceed it whatever the peer actually sends.

use tiamot_core::proto::{MAX_MESSAGE_BYTES, ProtocolError};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// Bytes of length prefix.
const PREFIX_BYTES: usize = 4;

/// A framed read or write failed.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    /// The stream ended or errored.
    #[error("connection stream failed")]
    Io(#[from] std::io::Error),

    /// The peer declared a message larger than the protocol allows.
    ///
    /// Refused on the strength of the declaration alone — nothing is allocated
    /// and no body is read.
    #[error("peer declared a {declared}-byte message, over the {MAX_MESSAGE_BYTES}-byte limit")]
    Oversized {
        /// What the peer claimed.
        declared: usize,
    },

    /// The peer declared a zero-length message.
    #[error("peer declared an empty message")]
    Empty,

    /// The body did not decode.
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
}

impl FrameError {
    /// Whether this ended the connection normally rather than badly.
    ///
    /// A peer closing a stream is not an error worth logging at warn level; a
    /// peer sending a four-gigabyte length prefix is.
    #[must_use]
    pub fn is_clean_close(&self) -> bool {
        matches!(
            self,
            Self::Io(err) if err.kind() == std::io::ErrorKind::UnexpectedEof
        )
    }
}

/// Reads one length-prefixed message from a stream.
///
/// # Errors
///
/// [`FrameError`] on a stream failure, an oversized declaration, or a body that
/// does not decode.
pub async fn read<R, T>(stream: &mut R) -> Result<T, FrameError>
where
    R: tokio::io::AsyncRead + Unpin,
    T: serde::de::DeserializeOwned,
{
    let mut prefix = [0u8; PREFIX_BYTES];
    stream.read_exact(&mut prefix).await?;
    let declared = u32::from_be_bytes(prefix) as usize;

    // Before the allocation, not after. See the module docs.
    if declared == 0 {
        return Err(FrameError::Empty);
    }
    if declared > MAX_MESSAGE_BYTES {
        return Err(FrameError::Oversized { declared });
    }

    let mut body = vec![0u8; declared];
    stream.read_exact(&mut body).await?;
    Ok(tiamot_core::proto::decode(&body)?)
}

/// Writes one length-prefixed message to a stream.
///
/// # Errors
///
/// [`FrameError`] if encoding or the write fails.
pub async fn write<W, T>(stream: &mut W, message: &T) -> Result<(), FrameError>
where
    W: tokio::io::AsyncWrite + Unpin,
    T: serde::Serialize,
{
    let body = tiamot_core::proto::encode(message)?;
    // `encode` already enforces the cap, so this cast cannot truncate.
    let prefix = u32::try_from(body.len()).map_err(|_| FrameError::Oversized {
        declared: body.len(),
    })?;
    stream.write_all(&prefix.to_be_bytes()).await?;
    stream.write_all(&body).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiamot_core::proto::{ClientMessage, PROTOCOL_VERSION};

    fn hello() -> ClientMessage {
        ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            public_key: [7u8; 32],
            display_name: "Alice".to_owned(),
        }
    }

    #[tokio::test]
    async fn a_message_round_trips() {
        let mut buffer = Vec::new();
        write(&mut buffer, &hello()).await.expect("write");

        let mut cursor = std::io::Cursor::new(buffer);
        let read: ClientMessage = read(&mut cursor).await.expect("read");
        assert_eq!(read, hello());
    }

    #[tokio::test]
    async fn several_messages_round_trip_in_order() {
        // The reason framing exists: a stream carries them back to back with no
        // natural boundary.
        let mut buffer = Vec::new();
        write(&mut buffer, &hello()).await.expect("write");
        write(&mut buffer, &ClientMessage::JoinWorld)
            .await
            .expect("write");
        write(&mut buffer, &ClientMessage::Disconnect)
            .await
            .expect("write");

        let mut cursor = std::io::Cursor::new(buffer);
        assert_eq!(
            read::<_, ClientMessage>(&mut cursor).await.expect("1"),
            hello()
        );
        assert_eq!(
            read::<_, ClientMessage>(&mut cursor).await.expect("2"),
            ClientMessage::JoinWorld
        );
        assert_eq!(
            read::<_, ClientMessage>(&mut cursor).await.expect("3"),
            ClientMessage::Disconnect
        );
    }

    #[tokio::test]
    async fn an_oversized_declaration_is_refused_without_allocating() {
        // The hostile case: four bytes of prefix claiming four gigabytes. The
        // body is never read, so the peer cannot make the server reserve it.
        let mut buffer = u32::MAX.to_be_bytes().to_vec();
        // Deliberately no body at all — if the implementation tried to read one
        // it would block or error on EOF instead of refusing the declaration.
        buffer.extend_from_slice(b"x");

        let mut cursor = std::io::Cursor::new(buffer);
        let err = read::<_, ClientMessage>(&mut cursor)
            .await
            .expect_err("must refuse");
        assert!(
            matches!(err, FrameError::Oversized { declared } if declared == u32::MAX as usize),
            "expected an oversized refusal, got {err}"
        );
    }

    #[tokio::test]
    async fn a_declaration_one_byte_over_the_limit_is_refused() {
        // The boundary. An off-by-one here means the cap is not the cap.
        let declared = u32::try_from(MAX_MESSAGE_BYTES + 1).expect("fits");
        let buffer = declared.to_be_bytes().to_vec();
        let mut cursor = std::io::Cursor::new(buffer);
        assert!(matches!(
            read::<_, ClientMessage>(&mut cursor).await,
            Err(FrameError::Oversized { .. })
        ));
    }

    #[tokio::test]
    async fn a_zero_length_declaration_is_refused() {
        let buffer = 0u32.to_be_bytes().to_vec();
        let mut cursor = std::io::Cursor::new(buffer);
        assert!(matches!(
            read::<_, ClientMessage>(&mut cursor).await,
            Err(FrameError::Empty)
        ));
    }

    #[tokio::test]
    async fn a_truncated_body_is_an_error_not_a_hang() {
        let mut buffer = Vec::new();
        write(&mut buffer, &hello()).await.expect("write");
        buffer.truncate(buffer.len() - 2);

        let mut cursor = std::io::Cursor::new(buffer);
        let err = read::<_, ClientMessage>(&mut cursor)
            .await
            .expect_err("must not succeed");
        assert!(
            err.is_clean_close() || matches!(err, FrameError::Io(_)),
            "{err}"
        );
    }

    #[tokio::test]
    async fn garbage_in_the_body_is_a_protocol_error_not_a_panic() {
        // Charter rule 14: every decoder treats its input as adversarial.
        let body = vec![0xFFu8; 64];
        let mut buffer = u32::try_from(body.len())
            .expect("fits")
            .to_be_bytes()
            .to_vec();
        buffer.extend_from_slice(&body);

        let mut cursor = std::io::Cursor::new(buffer);
        assert!(matches!(
            read::<_, ClientMessage>(&mut cursor).await,
            Err(FrameError::Protocol(_))
        ));
    }

    #[tokio::test]
    async fn an_empty_stream_reports_a_clean_close() {
        let mut cursor = std::io::Cursor::new(Vec::new());
        let err = read::<_, ClientMessage>(&mut cursor)
            .await
            .expect_err("nothing to read");
        assert!(
            err.is_clean_close(),
            "a peer that simply went away is not a protocol violation: {err}"
        );
    }
}
