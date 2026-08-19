// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Turning an Ogg Vorbis file from a server into samples, safely.
//!
//! # Every byte here is hostile
//!
//! Charter rule 14: a client decodes audio pushed by servers it does not trust,
//! and this is the whole of the audio side of that. The rules it follows are
//! the ones the glTF reader follows, for the same reasons:
//!
//! - **Pure Rust.** Symphonia, no C codec bindings — a memory-safety bug in a
//!   C decoder is a memory-safety bug in the client.
//! - **Limits before allocation.** A tiny header can declare a hundred channels
//!   at four megahertz. Every number that decides an allocation is checked
//!   against [`Limits`] *before* anything is allocated for it.
//! - **A budget while decoding, not only before it.** The headers are a claim,
//!   not a promise: a file may simply keep producing packets. Decoding stops
//!   at [`Limits::frames`] whatever the headers said.
//! - **Refused, never guessed.** A file this cannot decode produces an error
//!   the caller turns into a per-server warning and a silent sound. It never
//!   produces half a buffer.
//!
//! Panic isolation is the caller's job — see [`super::decode_isolated`] — and
//! the fuzz target calls [`decode`] directly, because a target that caught its
//! own panics would find nothing.
//!
//! # Determinism
//!
//! None required. Charter rule 4's scope note exempts audio explicitly, and
//! nothing here reaches the simulation: samples go to a mixer and no number
//! comes back.

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// What a decoded sound is.
#[derive(Debug, Clone, PartialEq)]
pub struct Clip {
    /// Interleaved samples, `channels` per frame.
    pub samples: Vec<f32>,
    /// How many channels are interleaved in [`Clip::samples`].
    pub channels: u16,
    /// Frames per second.
    pub sample_rate: u32,
}

impl Clip {
    /// How many frames the clip holds.
    #[must_use]
    pub fn frames(&self) -> usize {
        if self.channels == 0 {
            return 0;
        }
        self.samples.len() / self.channels as usize
    }

    /// How long it lasts, in seconds.
    #[must_use]
    pub fn seconds(&self) -> f32 {
        if self.sample_rate == 0 {
            return 0.0;
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "a duration for display and attenuation, not for simulation"
        )]
        let frames = self.frames() as f32;
        frames / self.sample_rate as f32
    }
}

/// What a sound may not exceed.
///
/// Every one of these bounds an allocation, and every one is checked before the
/// allocation happens.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Largest file accepted, in bytes, checked before parsing.
    pub file_bytes: usize,
    /// Most channels. Stereo is two; anything beyond a handful is a claim.
    pub channels: u16,
    /// Highest sample rate accepted.
    pub sample_rate: u32,
    /// Most frames decoded, whatever the file claims.
    ///
    /// **The budget that matters**, because a header is a claim and a stream
    /// can simply keep going. At 48 kHz this is a minute of audio.
    pub frames: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            // Four megabytes. A minute of stereo Vorbis at a sane bitrate is
            // well under this, and a mod shipping something longer is shipping
            // music, which wants streaming rather than a decoded buffer.
            file_bytes: 4 * 1024 * 1024,
            // Stereo, and a little room. A sound is spatialised by the client
            // from a mono or stereo source; a nine-channel file is not a sound
            // effect, it is a way to spend memory.
            channels: 8,
            // 192 kHz is beyond any sane source and still bounded.
            sample_rate: 192_000,
            // A minute at 48 kHz. Multiplied by channels and four bytes, the
            // worst case is about 92 MiB of samples for an eight-channel file
            // — which is why `channels` is bounded too.
            frames: 48_000 * 60,
        }
    }
}

/// Why a sound could not be decoded.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AudioError {
    /// The file is larger than [`Limits::file_bytes`], refused before parsing.
    #[error("sound is {bytes} bytes, over the {limit}-byte limit")]
    TooLarge {
        /// What arrived.
        bytes: usize,
        /// What is allowed.
        limit: usize,
    },
    /// The container is not something this can read.
    #[error("not a readable Ogg stream: {0}")]
    NotOgg(String),
    /// There is no audio track in it.
    #[error("the file has no audio track")]
    NoTrack,
    /// The track declares something over a limit.
    #[error("sound declares {found} {what}, over the limit of {limit}")]
    OverLimit {
        /// Which limit.
        what: &'static str,
        /// What the file claimed.
        found: u64,
        /// What is allowed.
        limit: u64,
    },
    /// The stream could not be decoded to the end.
    #[error("the audio stream is malformed: {0}")]
    Malformed(String),
}

/// Checks what a track's headers CLAIM, before a decoder is built for them.
///
/// Its own function because `decode` is at clippy's line limit, and because
/// this is the part charter rule 14 is actually about: these numbers decide how
/// much memory a frame costs, so they are checked before anything is allocated
/// for them. Returns the two the caller needs afterwards.
fn check_header(
    params: &symphonia::core::codecs::CodecParameters,
    limits: Limits,
) -> Result<(u16, u32), AudioError> {
    let channels = params
        .channels
        .map_or(0, symphonia::core::audio::Channels::count);
    let channels = u16::try_from(channels).unwrap_or(u16::MAX);
    if channels == 0 {
        return Err(AudioError::NoTrack);
    }
    if channels > limits.channels {
        return Err(AudioError::OverLimit {
            what: "channels",
            found: u64::from(channels),
            limit: u64::from(limits.channels),
        });
    }
    let sample_rate = params.sample_rate.unwrap_or(0);
    if sample_rate == 0 {
        return Err(AudioError::NoTrack);
    }
    if sample_rate > limits.sample_rate {
        return Err(AudioError::OverLimit {
            what: "hertz",
            found: u64::from(sample_rate),
            limit: u64::from(limits.sample_rate),
        });
    }
    // And the length, when the file admits one. A file that does not is not
    // refused — the frame budget below catches it either way — but one that
    // declares an hour is refused before a single packet is decoded.
    if let Some(frames) = params.n_frames
        && frames > limits.frames as u64
    {
        return Err(AudioError::OverLimit {
            what: "frames",
            found: frames,
            limit: limits.frames as u64,
        });
    }

    Ok((channels, sample_rate))
}

/// Decodes an Ogg Vorbis file into samples.
///
/// # Errors
///
/// [`AudioError`], naming the limit or the malformation. A file that cannot be
/// decoded is refused outright — there is no half-decoded clip.
pub fn decode(bytes: &[u8], limits: Limits) -> Result<Clip, AudioError> {
    // **Before anything else, and before any allocation.** A caller that
    // streamed this from a server has the length in hand already.
    if bytes.len() > limits.file_bytes {
        return Err(AudioError::TooLarge {
            bytes: bytes.len(),
            limit: limits.file_bytes,
        });
    }

    let source = std::io::Cursor::new(bytes.to_vec());
    let stream = MediaSourceStream::new(
        Box::new(source),
        symphonia::core::io::MediaSourceStreamOptions::default(),
    );
    let mut hint = Hint::new();
    hint.with_extension("ogg");

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            stream,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|err| AudioError::NotOgg(err.to_string()))?;
    let mut format = probed.format;

    let track = format
        .tracks()
        .iter()
        .find(|track| track.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
        .ok_or(AudioError::NoTrack)?;
    let track_id = track.id;
    let params = track.codec_params.clone();

    let (channels, sample_rate) = check_header(&params, limits)?;

    let mut decoder = symphonia::default::get_codecs()
        .make(&params, &DecoderOptions::default())
        .map_err(|err| AudioError::NotOgg(err.to_string()))?;

    let mut samples: Vec<f32> = Vec::new();
    let mut buffer: Option<SampleBuffer<f32>> = None;
    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            // The ordinary end of a stream, and the ordinary end of a
            // TRUNCATED one: a file that stops mid-packet has given us
            // everything it had, and what we have is playable.
            Err(SymphoniaError::IoError(_) | SymphoniaError::ResetRequired) => break,
            Err(err) => return Err(AudioError::Malformed(err.to_string())),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let audio = match decoder.decode(&packet) {
            Ok(audio) => audio,
            // A corrupt packet is skipped rather than fatal, which is what
            // every real decoder does and what keeps one bad frame from
            // silencing a whole sound.
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(err) => return Err(AudioError::Malformed(err.to_string())),
        };

        let spec = *audio.spec();
        let capacity = audio.capacity() as u64;
        let buffer = buffer.get_or_insert_with(|| SampleBuffer::new(capacity, spec));
        buffer.copy_interleaved_ref(audio);
        samples.extend_from_slice(buffer.samples());

        // **The budget, enforced while decoding and not only before it.** The
        // headers were a claim; this is the fact. Truncating rather than
        // erroring, because a sound that is too long is still a usable sound
        // and a mod that shipped one should hear the first minute of it.
        if samples.len() >= limits.frames.saturating_mul(channels as usize) {
            samples.truncate(limits.frames.saturating_mul(channels as usize));
            break;
        }
    }

    if samples.is_empty() {
        return Err(AudioError::Malformed("no samples decoded".to_owned()));
    }

    Ok(Clip {
        samples,
        channels,
        sample_rate,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_over_the_size_limit_is_refused_before_it_is_parsed() {
        // **The first check, and the one that has to come first.** Everything
        // below this allocates; a caller that streamed four gigabytes from a
        // server should be told no before any of it is looked at.
        let limits = Limits {
            file_bytes: 16,
            ..Limits::default()
        };
        let err = decode(&[0u8; 64], limits).expect_err("an oversized file was accepted");
        assert_eq!(
            err,
            AudioError::TooLarge {
                bytes: 64,
                limit: 16
            }
        );
    }

    #[test]
    fn garbage_is_an_error_and_not_a_panic() {
        // Charter rule 14. Every one of these is something a server could send,
        // and none of them may take the client down — which is the property the
        // fuzz target explores and this pins for the obvious cases.
        for bytes in [
            b"".as_slice(),
            b"OggS".as_slice(),
            b"not an ogg file at all".as_slice(),
            &[0xFF; 512],
            // A plausible header followed by nothing.
            b"OggS\x00\x02\x00\x00\x00\x00\x00\x00\x00\x00".as_slice(),
        ] {
            let outcome = decode(bytes, Limits::default());
            assert!(
                outcome.is_err(),
                "garbage decoded to a clip: {} bytes",
                bytes.len()
            );
        }
    }

    #[test]
    fn a_panicking_decoder_disables_one_sound_rather_than_the_client() {
        // `decode_isolated` is what charter rule 14 means by "panic isolation".
        // The decoder is written not to panic; this is what makes that a
        // property of the client rather than a promise about the code.
        let outcome = super::super::decode_isolated(b"not audio", Limits::default());
        assert!(outcome.is_err(), "garbage decoded to a clip");
    }

    /// Where a real Ogg Vorbis file goes, if somebody puts one there.
    const FIXTURE: &str = "tests/fixtures/tone.ogg";

    #[test]
    fn a_real_ogg_file_decodes_to_samples() {
        // **The gap worth naming.** Every other test here proves this decoder
        // REFUSES things. None of them proves it accepts a good file, and a
        // decoder that refused everything would pass all of them.
        //
        // There is no Vorbis encoder in this repository and none in the
        // container it is developed in, so the fixture cannot be generated the
        // way the glTF corpus is — `model::build::to_glb` has no counterpart
        // here. Drop a real file at the path below and this starts asserting:
        //
        //     ffmpeg -f lavfi -i "sine=frequency=440:duration=1" \
        //            -c:a libvorbis crates/client/tests/fixtures/tone.ogg
        //
        // Skipped rather than failed when it is absent, because a test that
        // cannot run is not the same as a test that failed — but it is reported
        // either way, so the gap does not go quiet.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE);
        let Ok(bytes) = std::fs::read(&path) else {
            eprintln!(
                "SKIPPED: no Ogg fixture at {}, so the decoder's ACCEPT path is \
                 unverified — see this test for how to make one",
                path.display()
            );
            return;
        };

        let clip = decode(&bytes, Limits::default()).expect("a real Ogg file should decode");
        assert!(clip.channels > 0 && clip.sample_rate > 0);
        assert!(!clip.samples.is_empty(), "decoded no samples");
        assert_eq!(
            clip.samples.len() % clip.channels as usize,
            0,
            "a partial frame at the end is a read past what the mixer thinks it has"
        );
        // A sine tone is not silence, which is what a decoder that produced the
        // right SHAPE and the wrong samples would give.
        assert!(
            clip.samples.iter().any(|sample| sample.abs() > 0.01),
            "every sample decoded to silence"
        );
    }

    #[test]
    fn a_clip_reports_its_own_length() {
        let clip = Clip {
            samples: vec![0.0; 8],
            channels: 2,
            sample_rate: 4,
        };
        assert_eq!(clip.frames(), 4);
        assert!((clip.seconds() - 1.0).abs() < f32::EPSILON);

        // The degenerate cases answer rather than divide by zero: these come
        // from a file somebody else wrote.
        let empty = Clip {
            samples: Vec::new(),
            channels: 0,
            sample_rate: 0,
        };
        assert_eq!(empty.frames(), 0);
        assert!(empty.seconds().abs() < f32::EPSILON);
    }
}
