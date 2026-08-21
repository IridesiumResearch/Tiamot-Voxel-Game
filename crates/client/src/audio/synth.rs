// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Making audio, so the engine does not have to commit any.
//!
//! # Why this exists
//!
//! Two holes, one shape. There is no Vorbis encoder here, so:
//!
//! 1. **Nothing could prove the decoder ACCEPTS a good file.** Every test in
//!    [`super::ingest`] proves it refuses things, and a decoder that refused
//!    everything would pass all of them.
//! 2. **The reference mods had no sounds to play**, so the audio path could be
//!    complete and still silent.
//!
//! A WAV is a header and some samples, so both are answered by writing one. It
//! is the same argument that made `engine:humanoid` a table of measurements
//! rather than a committed `.glb`: a binary blob in a repository is not source,
//! and a sound you can read the shape of is worth more than one you cannot.
//!
//! These are **fixtures, not content**. The reference mods are test fixtures
//! (see `game/README.md`) and so are their noises; a real game's sounds are
//! recordings, shipped by whoever makes that game.
//!
//! Charter rule 4 does not reach any of this — audio is presentation.

/// A sound to synthesise, described rather than recorded.
#[derive(Debug, Clone, Copy)]
pub struct Recipe {
    /// Starting frequency, in hertz.
    pub from_hz: f32,
    /// Frequency at the end. Equal to `from_hz` for a steady tone.
    pub to_hz: f32,
    /// How long, in seconds.
    pub seconds: f32,
    /// How much of the tone is replaced by noise, `0.0` to `1.0`.
    ///
    /// A pure tone is a beep. Real impacts are mostly noise, so a thud wants
    /// most of this and a UI click wants none.
    pub noise: f32,
    /// How sharply it decays. Larger is shorter and more percussive.
    pub decay: f32,
}

impl Recipe {
    /// A dull impact: breaking or placing a block.
    #[must_use]
    pub const fn thud() -> Self {
        Self {
            from_hz: 180.0,
            to_hz: 90.0,
            seconds: 0.18,
            noise: 0.65,
            decay: 18.0,
        }
    }

    /// A short scuff: a footstep.
    #[must_use]
    pub const fn step() -> Self {
        Self {
            from_hz: 320.0,
            to_hz: 180.0,
            seconds: 0.09,
            noise: 0.85,
            decay: 40.0,
        }
    }

    /// Liquid: a pour or a splash.
    #[must_use]
    pub const fn splash() -> Self {
        Self {
            from_hz: 700.0,
            to_hz: 1400.0,
            seconds: 0.25,
            noise: 0.55,
            decay: 12.0,
        }
    }

    /// A steady bed: ambience, meant to be looped.
    ///
    /// **Loopable, which constrains it more than it sounds.** A tone whose
    /// waveform does not line up at the seam clicks once per loop, and once per
    /// loop is exactly the interval at which a click becomes maddening. Almost
    /// pure noise has no waveform to line up, so the seam is inaudible — which
    /// is why ambience beds in real games are noise and not tones.
    ///
    /// No decay, or it would fade to nothing and loop from silence into full
    /// volume.
    #[must_use]
    pub const fn bed(from_hz: f32, to_hz: f32) -> Self {
        Self {
            from_hz,
            to_hz,
            seconds: 2.0,
            noise: 0.97,
            decay: 0.0,
        }
    }

    /// Daytime: a bright, airy bed.
    #[must_use]
    pub const fn day() -> Self {
        Self::bed(900.0, 1100.0)
    }

    /// Night: lower and darker, the same shape.
    #[must_use]
    pub const fn night() -> Self {
        Self::bed(240.0, 300.0)
    }

    /// A clean tick: the interface.
    #[must_use]
    pub const fn click() -> Self {
        Self {
            from_hz: 900.0,
            to_hz: 900.0,
            seconds: 0.04,
            noise: 0.0,
            decay: 60.0,
        }
    }
}

/// Sample rate everything here is written at.
///
/// 48 kHz because it is what every modern device runs at, so nothing has to
/// resample a fixture.
pub const SAMPLE_RATE: u32 = 48_000;

/// Renders a recipe to mono samples in `-1.0..=1.0`.
///
/// The noise is a fixed-seed LCG rather than anything from the system: the same
/// recipe gives the same bytes every time, so a regenerated fixture does not
/// show up as a diff. Nothing here needs to be deterministic across MACHINES —
/// audio is presentation — but a build step whose output churned would be a
/// build step nobody would run.
#[must_use]
#[expect(
    clippy::disallowed_methods,
    reason = "audio is presentation; float-determinism.md Scope"
)]
pub fn render(recipe: Recipe) -> Vec<f32> {
    let frames = ((recipe.seconds.max(0.0)) * SAMPLE_RATE as f32) as usize;
    let mut samples = Vec::with_capacity(frames);
    let mut noise_state: u32 = 0x1234_5678;
    let mut phase = 0.0_f32;

    for index in 0..frames {
        let t = index as f32 / SAMPLE_RATE as f32;
        let progress = if frames <= 1 {
            0.0
        } else {
            index as f32 / frames as f32
        };

        // A frequency sweep, integrated into the phase rather than applied to
        // `t` — multiplying a moving frequency by absolute time makes the pitch
        // jump backwards partway through.
        let hz = recipe.from_hz + (recipe.to_hz - recipe.from_hz) * progress;
        phase += std::f32::consts::TAU * hz / SAMPLE_RATE as f32;
        let tone = phase.sin();

        // xorshift: cheap, and the same every run.
        noise_state ^= noise_state << 13;
        noise_state ^= noise_state >> 17;
        noise_state ^= noise_state << 5;
        let noise = (noise_state as f32 / u32::MAX as f32) * 2.0 - 1.0;

        let mixed = tone * (1.0 - recipe.noise) + noise * recipe.noise;
        // Exponential decay, and a two-millisecond fade IN: a waveform that
        // starts at full amplitude clicks, and the click is louder than the
        // sound.
        let envelope = (-recipe.decay * t).exp() * (t / 0.002).min(1.0);
        samples.push((mixed * envelope).clamp(-1.0, 1.0));
    }
    samples
}

/// Wraps samples in a 16-bit mono WAV.
///
/// The smallest container Symphonia will read, written by hand because pulling
/// an encoder in to make four fixtures would be a dependency for nothing.
#[must_use]
pub fn to_wav(samples: &[f32]) -> Vec<u8> {
    let data_len = samples.len() * 2;
    let mut out = Vec::with_capacity(44 + data_len);
    let riff_len = u32::try_from(36 + data_len).unwrap_or(u32::MAX);

    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&riff_len.to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // PCM header size
    out.extend_from_slice(&1u16.to_le_bytes()); // format: integer PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // channels: mono
    out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    out.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes()); // bytes per second
    out.extend_from_slice(&2u16.to_le_bytes()); // bytes per frame
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&u32::try_from(data_len).unwrap_or(u32::MAX).to_le_bytes());
    for sample in samples {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "clamped to i16's range on the line above"
        )]
        let scaled = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
        out.extend_from_slice(&scaled.to_le_bytes());
    }
    out
}

/// A recipe straight to a playable file.
#[must_use]
pub fn wav(recipe: Recipe) -> Vec<u8> {
    to_wav(&render(recipe))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{Limits, ingest};

    #[test]
    fn a_synthesised_sound_decodes_back_to_what_went_in() {
        // **The hole this module was written to close.** Every other test in
        // `ingest` proves the decoder REFUSES things; a decoder that refused
        // everything would pass all of them. This is the round trip: samples
        // in, a real container, samples out.
        let recipe = Recipe::thud();
        let bytes = wav(recipe);
        let clip =
            ingest::decode(&bytes, Limits::default()).expect("a synthesised WAV should decode");

        assert_eq!(clip.channels, 1);
        assert_eq!(clip.sample_rate, SAMPLE_RATE);
        assert!(
            (clip.seconds() - recipe.seconds).abs() < 0.01,
            "decoded {} seconds, expected about {}",
            clip.seconds(),
            recipe.seconds
        );
        // And it is a SOUND, not silence — which is what a decoder that got the
        // shape right and the samples wrong would give.
        assert!(
            clip.samples.iter().any(|sample| sample.abs() > 0.05),
            "every sample decoded to silence"
        );
    }

    #[test]
    fn a_rendered_sound_starts_quietly_and_ends_quieter() {
        // The envelope, which is the difference between a sound and a click.
        let samples = render(Recipe::thud());
        assert!(!samples.is_empty());
        assert!(
            samples[0].abs() < 0.05,
            "the waveform starts at full amplitude, which clicks: {}",
            samples[0]
        );
        let last = samples[samples.len() - 1].abs();
        let middle = samples[samples.len() / 8].abs();
        assert!(
            last < middle,
            "the sound did not decay: {last} at the end against {middle} near the start"
        );
    }

    #[test]
    fn the_same_recipe_renders_the_same_bytes_every_time() {
        // Not a determinism requirement — audio is presentation — but a build
        // step whose output churned is a build step nobody would run.
        assert_eq!(wav(Recipe::step()), wav(Recipe::step()));
    }
}
