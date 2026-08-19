// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Audio: decoding what a server sends, and playing it.
//!
//! Charter rule 3 puts every audio crate in this crate and nowhere else. The
//! server never learns that a speaker exists — it says what happened and where
//! (`core::sound`), and everything about how that becomes a noise is here.

pub mod ingest;
pub mod mixer;
pub mod synth;

pub use ingest::{AudioError, Clip, Limits};
pub use mixer::{Bus, Mixer, Placement, Volumes, place};

/// Decodes a sound with panic isolation.
///
/// **Charter rule 14's "decoding on a worker with panic isolation".** A
/// poisoned asset disables that asset and nothing else: the caller gets an
/// error, the client shows a per-server warning, and the rest of the session
/// carries on. A panic inside a decoder is exactly the case this exists for —
/// [`ingest::decode`] is written not to panic, and this is what makes that a
/// property of the client rather than a promise about the code.
///
/// The fuzz target deliberately calls [`ingest::decode`] instead, because a
/// target that caught its own panics would find nothing.
///
/// # Errors
///
/// [`AudioError`] for a refusal, and [`AudioError::Malformed`] naming the panic
/// if one happened.
pub fn decode_isolated(bytes: &[u8], limits: Limits) -> Result<Clip, AudioError> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ingest::decode(bytes, limits)
    }))
    .unwrap_or_else(|_| {
        Err(AudioError::Malformed(
            "the decoder panicked; this sound is disabled for the session".to_owned(),
        ))
    })
}
