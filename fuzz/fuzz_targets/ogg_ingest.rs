// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Fuzzes the Ogg Vorbis decoder — the audio half of charter rule 14.
//!
//! A client decodes sound files pushed by servers it does not trust, and this
//! target lands in the same task the decoder does rather than being deferred to
//! hardening, which is what the charter asks for in as many words.
//!
//! The property is not "decoding succeeds" — almost every input here is
//! nonsense and should be refused. It is:
//!
//! 1. **It never panics.** `decode` is called directly rather than through
//!    `decode_isolated`, because a target that caught its own panics would
//!    find nothing. Symphonia is pure Rust and safe, so a panic here is a
//!    denial of service rather than a memory-safety bug — and a client that
//!    dies when a server sends a malformed sound is still a client that dies.
//! 2. **It obeys its limits.** A clip that came back over any of them is one
//!    the decoder was supposed to have refused before allocating for it. The
//!    frame budget is the one that matters: headers are a CLAIM, and a stream
//!    can simply keep producing packets.
//! 3. **What it returns is self-consistent.** `channels` divides the sample
//!    count exactly, because the mixer will read it in frames and a partial
//!    frame at the end is a read past what the caller thinks it has.
//!
//! **Seeding**: a fuzzer starting from random bytes spends its entire budget
//! failing the `OggS` magic check. There is no Vorbis ENCODER in this
//! repository — the trick the glTF target uses, of building a model in Rust and
//! emitting one, has no equivalent here — so the corpus wants a real `.ogg`
//! dropped into `fuzz/corpus/ogg_ingest/` before this finds anything deep.
//! Until then it explores the container parser, which is itself hostile input.
//!
//! Run: `cargo +nightly fuzz run ogg_ingest`
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let limits = client::audio::Limits::default();
    let Ok(clip) = client::audio::ingest::decode(data, limits) else {
        // A refusal is the expected outcome for almost everything, and is the
        // decoder working rather than failing.
        return;
    };

    assert!(
        clip.channels > 0 && clip.channels <= limits.channels,
        "decoded {} channels, over the limit of {}",
        clip.channels,
        limits.channels
    );
    assert!(
        clip.sample_rate > 0 && clip.sample_rate <= limits.sample_rate,
        "decoded at {} Hz, over the limit of {}",
        clip.sample_rate,
        limits.sample_rate
    );
    assert!(
        clip.frames() <= limits.frames,
        "decoded {} frames, over the budget of {} — the header's claim was \
         believed instead of the stream being counted",
        clip.frames(),
        limits.frames
    );
    assert!(
        clip.samples.len() % clip.channels as usize == 0,
        "decoded {} samples across {} channels, which leaves a partial frame \
         for the mixer to read past",
        clip.samples.len(),
        clip.channels
    );
});
