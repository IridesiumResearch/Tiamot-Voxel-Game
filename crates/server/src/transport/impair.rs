// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Deliberately bad networks, for tests that need one.
//!
//! # Why the harness fakes this
//!
//! Prediction and reconciliation are correct on loopback by construction: the
//! round trip is microseconds, so a client is barely ahead of the server, almost
//! nothing is ever in flight, and the input queue's reorder buffer and repeat
//! window never do anything. **Every bug in those mechanisms hides on
//! loopback**, and loopback is the only network the test suite has. Task 09's
//! test list names 150 ms and 5% loss because that is where the machinery
//! starts doing something.
//!
//! # Loss here is not packet loss
//!
//! QUIC retransmits, so a lost *packet* on a stream is invisible above the
//! transport — dropping those would test quinn rather than this engine. What
//! this drops is whole **messages**, at the application layer, which is the
//! failure the engine actually has to survive: an input that never arrives.
//!
//! # In `server` rather than in `core`
//!
//! Charter rule 3 scopes `core` to voxel data, simulation, scripting, physics,
//! persistence and protocol types. A network simulator is none of those. It
//! lives beside [`super::frame`] because that is what it wraps, and because
//! both the bot and the client already depend on this crate for exactly that
//! reason.

use tiamot_core::detgen::StreamRng;

use super::frame::{self, FrameError};

/// Artificial network conditions applied to outbound messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Impairment {
    /// One-way delay added before a message goes out, in milliseconds.
    ///
    /// One way, so a round trip is twice this — "150 ms of latency" in the
    /// usual sense is `latency_ms: 75` here. Named for what it does rather than
    /// for what a player would call it, because getting that backwards
    /// silently halves whatever is being tested.
    pub latency_ms: u64,
    /// Percentage of outbound messages dropped entirely, 0 to 100.
    pub loss_percent: u32,
    /// Seed for the loss draws.
    ///
    /// Fixed rather than random: a test that fails one run in twenty and cannot
    /// be re-run into the same failure is a test people delete.
    pub seed: u64,
}

impl Impairment {
    /// The conditions Task 09's test list names: 150 ms round trip, 5% loss.
    #[must_use]
    pub const fn task_09() -> Self {
        Self {
            latency_ms: 75,
            loss_percent: 5,
            seed: 0x7069_6e67,
        }
    }

    /// Whether anything is actually being impaired.
    #[must_use]
    pub const fn is_clean(&self) -> bool {
        self.latency_ms == 0 && self.loss_percent == 0
    }
}

/// A control stream that can be made to behave badly.
///
/// Wraps a [`quinn::SendStream`] and applies an [`Impairment`] to everything
/// written through it. Unimpaired — the normal case, and the only one in
/// production — it is a direct write and costs nothing.
///
/// # Latency is queued, not slept
///
/// **Sleeping before each write models the wrong thing.** A client sends
/// several inputs in a row per server state; a 75 ms sleep before each one
/// turns a burst into hundreds of milliseconds of wall clock, which is a link
/// that *spaces messages out* rather than one that delivers them late. The
/// sender falls permanently behind and the test measures the harness.
///
/// So a writer task takes the stream and writes each frame when its deadline
/// arrives. The delay is constant, so FIFO order is already deadline order and
/// nothing needs sorting. The task **owns** the stream rather than sharing it,
/// because two writers on one QUIC stream interleave frames and produce bytes
/// neither of them wrote.
pub struct Link {
    /// The stream, unless the writer task has taken it.
    direct: Option<quinn::SendStream>,
    /// Queue feeding the writer task, once latency is being simulated.
    delayed: Option<tokio::sync::mpsc::UnboundedSender<(tokio::time::Instant, Vec<u8>)>>,
    /// The writer task, aborted with this link.
    writer: Option<tokio::task::JoinHandle<()>>,
    impairment: Impairment,
    /// Draws for the loss decision. Seeded, so a failure reproduces.
    rng: StreamRng,
}

impl Drop for Link {
    fn drop(&mut self) {
        // The writer owns the stream, so leaving it running would hold the
        // stream open after the thing that owns it is gone.
        if let Some(writer) = self.writer.take() {
            writer.abort();
        }
    }
}

impl Link {
    /// Wraps a stream, unimpaired.
    #[must_use]
    pub fn new(stream: quinn::SendStream) -> Self {
        Self {
            direct: Some(stream),
            delayed: None,
            writer: None,
            impairment: Impairment::default(),
            rng: StreamRng::global(0, "link:loss"),
        }
    }

    /// Applies conditions to everything written from now on.
    pub fn impair(&mut self, impairment: Impairment) {
        self.rng = StreamRng::global(impairment.seed, "link:loss");
        self.impairment = impairment;

        if impairment.latency_ms == 0 || self.delayed.is_some() {
            return;
        }
        let Some(mut stream) = self.direct.take() else {
            return;
        };

        let (sender, mut receiver) =
            tokio::sync::mpsc::unbounded_channel::<(tokio::time::Instant, Vec<u8>)>();
        self.delayed = Some(sender);
        self.writer = Some(tokio::spawn(async move {
            while let Some((deadline, body)) = receiver.recv().await {
                tokio::time::sleep_until(deadline).await;
                let Ok(prefix) = u32::try_from(body.len()) else {
                    continue;
                };
                if stream.write_all(&prefix.to_be_bytes()).await.is_err()
                    || stream.write_all(&body).await.is_err()
                {
                    // The connection went away. Nothing to report to — the
                    // next read finds out.
                    return;
                }
            }
        }));
    }

    /// The conditions currently being simulated.
    #[must_use]
    pub const fn impairment(&self) -> Impairment {
        self.impairment
    }

    /// Writes a framed message, subject to the impairment.
    ///
    /// A dropped message returns `Ok`: from the sender's point of view it went
    /// out, which is exactly what a real lost message looks like.
    ///
    /// # Errors
    ///
    /// [`FrameError`] if encoding or the underlying write fails.
    pub async fn write<T: serde::Serialize>(&mut self, message: &T) -> Result<(), FrameError> {
        // Dropped BEFORE anything else, so a dropped message costs no time —
        // it never existed. Deciding to drop it after the delay would make a
        // lossy link slower than a clean one for a reason no real link has.
        if self.impairment.loss_percent > 0
            && self.rng.below(100) < u64::from(self.impairment.loss_percent)
        {
            return Ok(());
        }

        let Some(queue) = self.delayed.as_ref() else {
            if let Some(stream) = self.direct.as_mut() {
                frame::write(stream, message).await?;
            }
            return Ok(());
        };

        let body = tiamot_core::proto::encode(message)?;
        let deadline = tokio::time::Instant::now()
            + std::time::Duration::from_millis(self.impairment.latency_ms);
        queue.send((deadline, body)).map_err(|_| {
            FrameError::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "the delayed writer ended",
            ))
        })
    }

    /// Closes the stream, if this link still holds it.
    ///
    /// A no-op once a writer task owns it: the task is aborted on drop and the
    /// stream closes with it.
    pub fn finish(&mut self) {
        if let Some(stream) = self.direct.as_mut() {
            let _ = stream.finish();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_impairment_is_recognised_as_clean() {
        assert!(Impairment::default().is_clean());
        assert!(!Impairment::task_09().is_clean());
        assert!(
            !Impairment {
                latency_ms: 0,
                loss_percent: 1,
                seed: 0,
            }
            .is_clean(),
            "loss with no latency is still an impairment"
        );
    }

    #[test]
    fn the_task_09_conditions_are_one_way_halves_of_the_round_trip() {
        // The unit trap this type exists to avoid. 150 ms of latency means 75
        // ms each way, and writing 150 here would silently double it.
        let conditions = Impairment::task_09();
        assert_eq!(conditions.latency_ms * 2, 150);
        assert_eq!(conditions.loss_percent, 5);
    }

    #[test]
    fn the_loss_draw_is_reproducible_from_its_seed() {
        // What makes a failure re-runnable. Two links on the same seed must
        // drop the same messages.
        let draws = |seed: u64| {
            let mut rng = StreamRng::global(seed, "link:loss");
            (0..64).map(|_| rng.below(100) < 5).collect::<Vec<_>>()
        };
        assert_eq!(draws(7), draws(7), "the same seed drew differently");
        assert_ne!(
            draws(7),
            draws(8),
            "two seeds drew identically, so the seed is not being used"
        );
    }
}
