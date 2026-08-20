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

/// Walks a RIFF file's chunks, refusing anything whose structure is ambiguous.
///
/// # Why this is strict rather than clever
///
/// The obvious guard reads the format chunk and checks the two fields that
/// panic. Two fuzz findings in one session killed that idea: a file with a
/// SECOND `fmt ` chunk (the guard read the first, Symphonia read the second),
/// and then a file whose bogus chunk length derailed this walk while Symphonia
/// still reached a later `fmt `.
///
/// The lesson is that a guard which must agree with the decoder about which
/// bytes are the format chunk is a guard coupled to a dependency's internals,
/// and that coupling loses: any disagreement is a bypass, and a byte-exact
/// re-implementation of somebody else's parser is not a guard, it is a second
/// parser with its own bugs.
///
/// So this does not try to predict Symphonia. It insists the file leave nothing
/// to predict:
///
/// - the declared RIFF size accounts for the file exactly,
/// - the chunk walk lands exactly on the end rather than overrunning or
///   stopping short,
/// - there is exactly one `fmt ` chunk and at least one `data` chunk.
///
/// A file that satisfies all three has one possible reading, so Symphonia's
/// reading and this one are the same reading. Anything else is refused —
/// including files that are merely unusual rather than hostile, which is the
/// right trade for bytes a stranger sent.
///
/// Returns the sole format chunk's payload.
fn wav_format_chunk(bytes: &[u8]) -> Result<&[u8], AudioError> {
    let refuse = |why: &str| AudioError::NotOgg(format!("a WAV this does not accept: {why}"));

    // The declared size covers everything after "RIFF" and its own four bytes.
    let declared = bytes
        .get(4..8)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| refuse("no RIFF size"))? as usize;
    if declared != bytes.len() - 8 {
        return Err(refuse("the RIFF size does not match the file"));
    }

    let mut format: Option<&[u8]> = None;
    let mut formats = 0usize;
    let mut has_data = false;
    // Past "RIFF", the length, and "WAVE".
    let mut at = 12usize;
    while at < bytes.len() {
        // A remainder too short to hold a header is exactly the leftover this
        // refuses: the file ends in bytes that are not a chunk.
        let (Some(kind), Some(len)) = (
            bytes.get(at..at + 4),
            bytes
                .get(at + 4..at + 8)
                .and_then(|raw| raw.try_into().ok())
                .map(|raw| u32::from_le_bytes(raw) as usize),
        ) else {
            return Err(refuse("a chunk header runs past the end"));
        };
        let start = at + 8;
        let end = start
            .checked_add(len)
            .ok_or_else(|| refuse("a chunk length overflows"))?;
        let Some(payload) = bytes.get(start..end) else {
            return Err(refuse("a chunk runs past the end"));
        };
        if kind == b"fmt " {
            formats += 1;
            format = Some(payload);
        }
        if kind == b"data" {
            has_data = true;
        }
        // Chunks are word-aligned, and a zero length must still advance or this
        // walks the same header for ever.
        at = end
            .checked_add(len & 1)
            .ok_or_else(|| refuse("a chunk length overflows"))?;
    }

    // `at == len + 1` is the pad byte of a final odd-length chunk that the
    // writer left off. Common in the wild, and NOT ambiguous: there is no
    // further chunk either way, so both readings end in the same place.
    if at != bytes.len() && at != bytes.len() + 1 {
        return Err(refuse("the chunks do not account for the file exactly"));
    }
    if formats != 1 {
        return Err(refuse("there is not exactly one format chunk"));
    }
    if !has_data {
        return Err(refuse("there is no data chunk"));
    }
    format.ok_or_else(|| refuse("there is no format chunk"))
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

    // **The container is checked by hand before Symphonia sees the bytes.**
    //
    // Found by `fuzz/ogg_ingest` within a minute of its first CI run: an ID3v2
    // tag reaches `symphonia-metadata`, whose extended-header parser reads
    // `(restrictions & 0x40) >> 5` — which is 0 or **2** — into a match that
    // handles only 0 and 1, and calls `unreachable!()` otherwise. The mask
    // should be `0x20`. It is an upstream bug and a server could send it
    // deliberately.
    //
    // `decode_isolated` already contains the damage, but a panic caught is
    // still a decode worker torn down by a stranger. So this refuses anything
    // that is not one of the two containers actually decoded here, which also
    // makes the accepted set explicit rather than "whatever Symphonia probes
    // for" — a set that grows silently with every dependency bump.
    //
    // The same pre-validation the glTF reader uses, for the same reason:
    // catching a panic is not the same as not reaching one.
    let ogg = bytes.starts_with(b"OggS");
    let wav = bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WAVE");
    if !ogg && !wav {
        return Err(AudioError::NotOgg("not an Ogg or WAV container".to_owned()));
    }

    // **And the WAV structure, before Symphonia builds a `TimeBase` from it.**
    // Found by the fuzz target's second CI run: `TimeBase::new` panics outright
    // on a zero numerator or denominator, and a WAV header declaring zero
    // channels or zero hertz reaches it during the PROBE — before the header
    // checks further down get a look.
    //
    // Symphonia is pure Rust and memory-safe, so this is a denial of service
    // rather than a corruption. It is still a decode worker a stranger can tear
    // down at will.
    //
    // [`wav_format_chunk`] carries the reasoning for why this refuses an
    // ambiguous file outright instead of trying to read the same format chunk
    // Symphonia will. The short version: two findings in one session proved
    // that predicting another parser is a game the guard loses.
    if wav {
        let fmt = wav_format_chunk(bytes)?;
        let channels = fmt
            .get(2..4)
            .and_then(|bytes| bytes.try_into().ok())
            .map_or(0, u16::from_le_bytes);
        let rate = fmt
            .get(4..8)
            .and_then(|bytes| bytes.try_into().ok())
            .map_or(0, u32::from_le_bytes);
        if channels == 0 || rate == 0 {
            return Err(AudioError::NoTrack);
        }
    }

    let source = std::io::Cursor::new(bytes.to_vec());
    let stream = MediaSourceStream::new(
        Box::new(source),
        symphonia::core::io::MediaSourceStreamOptions::default(),
    );
    let mut hint = Hint::new();
    hint.with_extension(if ogg { "ogg" } else { "wav" });

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
    fn a_wav_whose_structure_is_ambiguous_is_refused() {
        // **Where the fuzz target's second and third findings landed.** Both
        // were files carrying two `fmt ` chunks, and both defeated a guard that
        // read "the" format chunk and checked its fields — first by putting the
        // zeroed one second, then by derailing the walk that looked for it.
        //
        // So the property is no longer "the guard finds the same chunk
        // Symphonia does". It is that a file with more than one reading is not
        // read at all.
        let good = crate::audio::synth::wav(crate::audio::synth::Recipe::click());
        assert!(
            decode(&good, Limits::default()).is_ok(),
            "an ordinary WAV was refused"
        );

        // A second format chunk, appended. Valid on its own terms; ambiguous.
        let mut second = Vec::new();
        second.extend_from_slice(b"fmt ");
        second.extend_from_slice(&16u32.to_le_bytes());
        second.extend_from_slice(&1u16.to_le_bytes()); // PCM
        second.extend_from_slice(&1u16.to_le_bytes()); // channels
        second.extend_from_slice(&0u32.to_le_bytes()); // ZERO hertz
        second.extend_from_slice(&0u32.to_le_bytes()); // byte rate
        second.extend_from_slice(&2u16.to_le_bytes()); // block align
        second.extend_from_slice(&16u16.to_le_bytes()); // bits per sample

        // Appended WITH the RIFF size corrected, so the refusal is provably
        // about the duplicate chunk rather than about a size mismatch.
        let mut doubled = good.clone();
        doubled.extend_from_slice(&second);
        let size = u32::try_from(doubled.len() - 8).expect("fixture is small");
        doubled[4..8].copy_from_slice(&size.to_le_bytes());
        assert!(
            decode(&doubled, Limits::default()).is_err(),
            "a WAV with two format chunks was accepted"
        );

        // Trailing bytes the chunk walk cannot account for. Three of them,
        // because eight zeroes ARE accountable — they read as an empty chunk
        // with an odd name, which has exactly one reading and is therefore
        // allowed. What is refused is a remainder too short to be a header.
        let mut trailing = good.clone();
        trailing.extend_from_slice(&[0u8; 3]);
        let size = u32::try_from(trailing.len() - 8).expect("fixture is small");
        trailing[4..8].copy_from_slice(&size.to_le_bytes());
        assert!(
            decode(&trailing, Limits::default()).is_err(),
            "a WAV with unaccounted trailing bytes was accepted"
        );

        // A final odd-length chunk whose pad byte the writer left off. Common
        // in the wild, unambiguous, and so accepted rather than refused.
        let mut unpadded = good.clone();
        unpadded.extend_from_slice(b"LIST");
        unpadded.extend_from_slice(&1u32.to_le_bytes());
        unpadded.push(0);
        let size = u32::try_from(unpadded.len() - 8).expect("fixture is small");
        unpadded[4..8].copy_from_slice(&size.to_le_bytes());
        assert!(
            decode(&unpadded, Limits::default()).is_ok(),
            "a WAV missing only its final pad byte was refused"
        );

        // A RIFF size that disagrees with the file it describes.
        let mut lying = good.clone();
        lying[4..8].copy_from_slice(&9999u32.to_le_bytes());
        assert!(
            decode(&lying, Limits::default()).is_err(),
            "a WAV whose declared size was wrong was accepted"
        );
    }

    #[test]
    fn every_reference_sound_still_decodes() {
        // **The other half of a strictness guard.** `wav_format_chunk` refuses
        // ambiguous files, and the way that fails is by growing strict enough
        // to refuse real ones. These are the four sounds the reference mods
        // actually ship, read off disk rather than synthesised, so tightening
        // the guard past what the project's own assets satisfy fails here
        // rather than in somebody's ears.
        for name in [
            "core_blocks/sounds/step.wav",
            "core_milk/sounds/pour.wav",
            "core_tools/sounds/break.wav",
            "core_tools/sounds/place.wav",
        ] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../game")
                .join(name);
            let bytes = std::fs::read(&path).expect("a reference sound is missing");
            assert!(
                decode(&bytes, Limits::default()).is_ok(),
                "{name} was refused by the ingest guard"
            );
        }
    }

    #[test]
    fn the_exact_inputs_the_fuzzer_crashed_on_are_refused() {
        // Both findings, kept as bytes rather than read from
        // `fuzz/corpus/ogg_ingest/` so this test does not depend on the fuzzer
        // being installed or its corpus being present. The same two files are
        // in the corpus as named `regression-*.wav` seeds so the fuzzer keeps
        // mutating around them.
        //
        // The first put a zeroed `fmt ` chunk SECOND; the second derailed the
        // chunk walk with a bogus length so it never reached the zeroed one at
        // all. Different bypasses, one guard, and neither is about the fields —
        // both files are simply ambiguous.
        let two_format_chunks: &[u8] = &[
            82, 73, 70, 70, 36, 15, 0, 0, 87, 65, 86, 69, 102, 109, 116, 32, 16, 0, 0, 0, 1, 0, 1,
            0, 128, 187, 0, 0, 0, 87, 65, 86, 188, 0, 16, 0, 100, 0, 0, 0, 9, 0, 0, 0, 0, 0, 0, 0,
            1, 0, 0, 0, 86, 69, 102, 109, 116, 32, 16, 0, 0, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0, 8, 1, 0,
            2, 0, 24, 0, 0, 0,
        ];
        let derailed_chunk_walk: &[u8] = &[
            82, 73, 70, 70, 0, 0, 36, 15, 87, 65, 86, 69, 102, 109, 116, 32, 18, 0, 0, 0, 6, 0, 1,
            0, 36, 87, 0, 0, 0, 83, 255, 255, 255, 73, 70, 70, 6, 0, 36, 15, 87, 65, 86, 69, 102,
            109, 116, 32, 18, 0, 0, 0, 6, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 36, 87, 0, 0,
            0, 83, 255, 255, 255, 65, 1, 129, 2, 0, 24, 0, 100, 79, 79, 79, 79, 254, 246, 190, 185,
        ];
        for (name, bytes) in [
            ("two format chunks", two_format_chunks),
            ("a derailed chunk walk", derailed_chunk_walk),
        ] {
            assert!(
                decode(bytes, Limits::default()).is_err(),
                "the input with {name} decoded to a clip"
            );
        }
    }

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
    fn a_container_this_does_not_decode_is_refused_before_it_is_parsed() {
        // **Found by the fuzz target on its first CI run.** An ID3v2 tag
        // reaches `symphonia-metadata`, whose extended-header parser computes
        // `(restrictions & 0x40) >> 5` — 0 or 2 — and matches only 0 and 1,
        // calling `unreachable!()` otherwise. Upstream's mask is wrong, and a
        // server could send that deliberately.
        //
        // `decode_isolated` contains it, but a panic caught is still a decode
        // worker torn down by a stranger, so the bytes never get there.
        for prefix in [
            b"ID3\x04\x00\x40".as_slice(),
            b"ID3\x03\x00\xff".as_slice(),
            b"fLaC".as_slice(),
            b"\xff\xfb".as_slice(),
            b"RIFFxxxxAVI ".as_slice(),
        ] {
            let mut bytes = prefix.to_vec();
            bytes.extend_from_slice(&[0u8; 128]);
            let err = decode(&bytes, Limits::default())
                .expect_err("a container this does not decode was accepted");
            assert!(
                matches!(err, AudioError::NotOgg(_)),
                "refused for the wrong reason: {err}"
            );
        }

        // And the two that ARE decoded still get through to a real parse,
        // which is what stops this check from being a way to refuse everything.
        let wav = crate::audio::synth::wav(crate::audio::synth::Recipe::click());
        assert!(decode(&wav, Limits::default()).is_ok(), "a WAV was refused");
        // A truncated Ogg is refused by the PARSER, not by the magic check —
        // a different error, which is how we know it got past this.
        let ogg = b"OggS\x00\x02\x00\x00\x00\x00";
        assert!(
            !matches!(
                decode(ogg, Limits::default()),
                Err(AudioError::NotOgg(ref reason)) if reason.contains("container")
            ),
            "an Ogg header was refused by the magic check rather than parsed"
        );
    }

    #[test]
    fn a_wav_header_declaring_nothing_is_refused_before_the_probe() {
        // **The fuzz target's second find.** `TimeBase::new` panics outright on
        // a zero numerator or denominator, and a WAV declaring zero channels or
        // zero hertz reaches it during the PROBE — before the header checks
        // further down get a look at anything.
        //
        // Built by corrupting a real file rather than by hand, so the rest of
        // the container stays valid and the refusal is provably about these two
        // fields rather than about something else being malformed.
        let good = crate::audio::synth::wav(crate::audio::synth::Recipe::click());
        assert!(
            decode(&good, Limits::default()).is_ok(),
            "the fixture is bad"
        );

        // The `fmt ` payload begins at byte 20: format, then channels at 22,
        // then the sample rate at 24.
        for (offset, width, label) in [(22usize, 2usize, "channels"), (24, 4, "hertz")] {
            let mut broken = good.clone();
            for byte in &mut broken[offset..offset + width] {
                *byte = 0;
            }
            assert!(
                decode(&broken, Limits::default()).is_err(),
                "a WAV declaring zero {label} was accepted, and Symphonia panics on it"
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
