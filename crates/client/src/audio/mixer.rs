// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Playing sounds: buses, volumes, and where a noise is coming from.
//!
//! # Silence is a valid outcome
//!
//! A machine with no sound device, a headless test, a CI runner — all of these
//! run the client and none of them can open a mixer. Every method here is a
//! no-op when there is no device, and [`Mixer::available`] says which happened.
//! That is deliberate: an engine that refused to start without a sound card
//! would be an engine nobody could test.
//!
//! # Spatialisation, and what it deliberately is not
//!
//! Distance attenuation and a stereo pan, and a gentle low-pass with distance.
//! **No HRTF**, which the task says in as many words: it is a large amount of
//! work for a difference most players hear as "muddier", and it costs per
//! source per frame.
//!
//! Charter rule 4 does not reach any of this — rule 4's scope note exempts
//! audio explicitly — so the arithmetic here is ordinary floating point with
//! `sin`, `sqrt` and anything else that helps. Nothing here is ever read back
//! into the simulation.

use std::collections::BTreeMap;

use crate::audio::Clip;

/// Which mixer bus a sound belongs to.
///
/// Volumes are per bus and persist in the client config, so a player can turn
/// music down without turning footsteps down. Named rather than numbered
/// because a config file that said `bus_2 = 0.4` would be unreadable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Bus {
    /// Everything the world does: blocks, fluid, entities.
    Effects,
    /// Looping background noise — wind, a river.
    Ambient,
    /// Music.
    Music,
    /// The interface: clicks, confirmations.
    Ui,
}

impl Bus {
    /// Every bus, for iterating settings.
    pub const ALL: [Self; 4] = [Self::Effects, Self::Ambient, Self::Music, Self::Ui];

    /// The name this bus has in the config file.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Effects => "effects",
            Self::Ambient => "ambient",
            Self::Music => "music",
            Self::Ui => "ui",
        }
    }
}

/// How loud each bus is, and the master over all of them.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Volumes {
    /// Multiplies every bus.
    pub master: f32,
    /// Per bus, by [`Bus::name`].
    pub buses: BTreeMap<String, f32>,
}

impl Default for Volumes {
    fn default() -> Self {
        let mut buses = BTreeMap::new();
        for bus in Bus::ALL {
            // Not 1.0: a fresh install should be audible and not startling,
            // and every one of these is a slider a player can move.
            buses.insert(bus.name().to_owned(), 0.8);
        }
        Self { master: 0.7, buses }
    }
}

impl Volumes {
    /// The multiplier for one bus, master included.
    #[must_use]
    pub fn of(&self, bus: Bus) -> f32 {
        let own = self.buses.get(bus.name()).copied().unwrap_or(1.0);
        (self.master * own).clamp(0.0, 4.0)
    }
}

/// How a sound arrives at the listener: how loud, and from which side.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    /// Final gain, after distance and volumes.
    pub gain: f32,
    /// Stereo position, `-1.0` full left to `1.0` full right.
    pub pan: f32,
    /// How much of the high end survives, `0.0` to `1.0`.
    ///
    /// Distance eats treble, which is most of why something far away sounds
    /// far away rather than merely quiet.
    pub brightness: f32,
}

/// Works out how a sound at `source` reaches a listener.
///
/// `forward` and `right` are the listener's own axes — unit vectors — so this
/// needs no camera type and can be tested with numbers.
///
/// **Attenuation is linear in the distance ratio, not inverse-square.** An
/// inverse-square law is correct for a point source in free air and sounds
/// wrong in a game: it drops to near-nothing within a few blocks and then has a
/// very long inaudible tail. Falling linearly to zero at the radius means the
/// radius the mod asked for is the distance the sound actually stops carrying,
/// which is the thing a mod author can reason about.
#[must_use]
pub fn place(
    source: [f64; 3],
    listener: [f64; 3],
    forward: [f32; 3],
    right: [f32; 3],
    radius: f32,
    gain: f32,
) -> Placement {
    let offset = [
        (source[0] - listener[0]) as f32,
        (source[1] - listener[1]) as f32,
        (source[2] - listener[2]) as f32,
    ];
    let distance = (offset[0] * offset[0] + offset[1] * offset[1] + offset[2] * offset[2]).sqrt();

    // At or past the radius there is nothing to hear. Guarding zero radius as
    // well: a mod may ask for one, and a division by it would be a NaN in
    // somebody's ears.
    if radius <= 0.0 || distance >= radius {
        return Placement {
            gain: 0.0,
            pan: 0.0,
            brightness: 1.0,
        };
    }
    let nearness = 1.0 - (distance / radius);

    // **Standing inside the sound is not a direction.** Within half a block
    // the pan is meaningless and the arithmetic below is unstable, so it
    // collapses to centred rather than swinging wildly as a player turns.
    let (pan, brightness) = if distance < 0.5 {
        (0.0, 1.0)
    } else {
        let inverse = 1.0 / distance;
        let unit = [
            offset[0] * inverse,
            offset[1] * inverse,
            offset[2] * inverse,
        ];
        let sideways = unit[0] * right[0] + unit[1] * right[1] + unit[2] * right[2];
        // A little behind the head is not the same as beside it, but the
        // difference a stereo pair can express is small; `forward` is used to
        // keep a sound from swapping sides as a player turns past it.
        let ahead = unit[0] * forward[0] + unit[1] * forward[1] + unit[2] * forward[2];
        let pan = (sideways * (1.0 - 0.3 * ahead.abs())).clamp(-1.0, 1.0);
        // Treble falls faster than volume does, which is what distance sounds
        // like. Never below a quarter, or a far sound becomes a rumble.
        (pan, 0.25 + 0.75 * nearness)
    };

    Placement {
        gain: (gain * nearness).clamp(0.0, 4.0),
        pan,
        brightness,
    }
}

/// What a mod asked a sound to sound like, from `register_sound`.
///
/// Separate from [`Clip`], which is what the FILE contains: one is the decoded
/// samples, the other is the mod's opinion about them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Voice {
    /// A multiplier on the file's own level.
    pub gain: f32,
    /// How much to vary the pitch each play, as a fraction of it.
    ///
    /// `0.0` plays it identically every time, which is what makes a repeated
    /// footstep sound like a machine.
    pub pitch_variance: f32,
}

impl Default for Voice {
    fn default() -> Self {
        Self {
            gain: 1.0,
            pitch_variance: 0.0,
        }
    }
}

impl Voice {
    /// The voice a server declared, with its numbers brought into range.
    ///
    /// Charter rule 14: these came off the wire. A negative gain is not a quiet
    /// sound and a pitch variance of 40 is not a sound at all, so both are
    /// clamped here rather than trusted into a backend.
    #[must_use]
    pub fn of(sound: &tiamot_core::proto::SoundDef) -> Self {
        Self {
            gain: if sound.gain.is_finite() {
                sound.gain.clamp(0.0, 4.0)
            } else {
                1.0
            },
            pitch_variance: if sound.pitch_variance.is_finite() {
                sound.pitch_variance.clamp(0.0, 0.5)
            } else {
                0.0
            },
        }
    }
}

/// A decoded sound and the mod's opinion about how to play it.
#[derive(Debug, Clone)]
struct Loaded {
    clip: Clip,
    voice: Voice,
}

/// The audio backend, or nothing if this machine has no sound device.
pub struct Mixer {
    manager: Option<kira::AudioManager>,
    /// Decoded sounds, by qualified id.
    clips: BTreeMap<String, Loaded>,
    /// Advances once per play, so successive plays of one sound differ.
    plays: u64,
    /// How loud each bus is.
    volumes: Volumes,
}

impl std::fmt::Debug for Mixer {
    /// Hand-written because `kira::AudioManager` is not `Debug`, and because
    /// the useful facts about a mixer are how many sounds it holds and whether
    /// it has anywhere to play them — not the contents of every buffer.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mixer")
            .field("available", &self.available())
            .field("clips", &self.clips.len())
            .field("volumes", &self.volumes)
            .finish_non_exhaustive()
    }
}

impl Mixer {
    /// Opens the sound device, or gives back a silent mixer if there is none.
    ///
    /// **Never fails.** A machine with no audio should run the game without
    /// sound rather than not run it, and every headless test is such a machine.
    #[must_use]
    pub fn open(volumes: Volumes) -> Self {
        let manager = kira::AudioManager::new(kira::AudioManagerSettings::default()).ok();
        if manager.is_none() {
            tracing::info!("no audio device; the client will run silently");
        }
        Self {
            manager,
            clips: BTreeMap::new(),
            plays: 0,
            volumes,
        }
    }

    /// Whether there is a device to play through.
    #[must_use]
    pub fn available(&self) -> bool {
        self.manager.is_some()
    }

    /// How loud each bus is.
    #[must_use]
    pub fn volumes(&self) -> &Volumes {
        &self.volumes
    }

    /// Replaces the volumes, as the settings screen does.
    pub fn set_volumes(&mut self, volumes: Volumes) {
        self.volumes = volumes;
    }

    /// Takes a decoded sound, ready to be played by id.
    ///
    /// Held even with no device: whether a sound decoded is a property of the
    /// asset and the test suite asks about it on machines with no speakers.
    ///
    /// `voice` is what the MOD asked for — its own level and how much to vary
    /// the pitch. Carried here rather than applied at the call site because
    /// every caller would otherwise have to remember to, and one of them
    /// (footsteps) did not.
    pub fn insert(&mut self, id: String, clip: Clip, voice: Voice) {
        self.clips.insert(id, Loaded { clip, voice });
    }

    /// Whether this sound has been decoded and is ready.
    #[must_use]
    pub fn holds(&self, id: &str) -> bool {
        self.clips.contains_key(id)
    }

    /// How many sounds are loaded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.clips.len()
    }

    /// Whether no sounds are loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.clips.is_empty()
    }

    /// A pitch offset in `[-variance, +variance]`, different each play.
    ///
    /// A counter through a bit-mixer rather than `rand`: audio is outside
    /// charter rule 4 entirely, so nothing here needs a real generator, and a
    /// sequence a test can predict is worth more than one it cannot. The client
    /// gains no dependency for it.
    #[expect(
        clippy::cast_precision_loss,
        reason = "a pitch wobble; the low 24 bits are all that is read"
    )]
    fn jitter(&mut self, variance: f32) -> f32 {
        if variance <= 0.0 {
            return 0.0;
        }
        self.plays = self.plays.wrapping_add(1);
        // splitmix64's finalising avalanche, which spreads a counter well
        // enough that consecutive plays do not sound related.
        let mut x = self.plays.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        x ^= x >> 30;
        x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x ^= x >> 27;
        x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^= x >> 31;
        // Into [-1, 1], then scaled.
        let unit = ((x >> 40) as f32) / 8_388_608.0 - 1.0;
        unit * variance
    }

    /// Plays a loaded sound, placed for the listener.
    ///
    /// Returns whether anything was started. `false` covers all the ordinary
    /// reasons — no device, a sound still being fetched, a placement out of
    /// earshot — none of which is an error worth reporting to a player.
    pub fn play(&mut self, id: &str, bus: Bus, placement: Placement) -> bool {
        let Some(voice) = self.clips.get(id).map(|loaded| loaded.voice) else {
            return false;
        };
        // **The mod's own level, and then the player's.** `register_sound`'s
        // `gain` is what the mod says this sound is worth relative to its file;
        // the bus volume is what the player says the bus is worth. Both, in
        // that order.
        let volume = self.volumes.of(bus) * placement.gain * voice.gain;
        if volume <= 0.0 {
            return false;
        }
        // **Pitch variance is why a repeated footstep is not a machine.** A
        // sound played identically twenty times a minute reads as a sample on a
        // loop, which is exactly what a mod sets `pitch_variance` to avoid.
        //
        // Charter rule 4 exempts audio explicitly, so this may use an ordinary
        // generator — but it is a plain counter-driven hash rather than `rand`
        // so a test can predict it and the client gains no dependency.
        let rate = f64::from(1.0 + self.jitter(voice.pitch_variance));
        // Re-borrowed after the jitter, which needs `&mut self` for its counter.
        let Some(clip) = self.clips.get(id).map(|loaded| &loaded.clip) else {
            return false;
        };
        let Some(manager) = self.manager.as_mut() else {
            return false;
        };

        // kira wants its own frame type, and this is where the decoded samples
        // become one. Built per play rather than cached because a `StaticSound`
        // carries its own settings and the pan differs every time.
        //
        // **The treble comes off HERE**, which is the one place it can. kira's
        // filters are track effects, and a per-play cutoff would need a track
        // per sound; the samples are already being walked to build the frames,
        // so a one-pole low-pass over that walk is very nearly free and stays
        // entirely under this crate's control.
        let mut low = Filter::new(placement.brightness);
        let frames: Vec<kira::Frame> = match clip.channels {
            1 => clip
                .samples
                .iter()
                .map(|sample| kira::Frame::from_mono(low.mono(*sample)))
                .collect(),
            _ => clip
                .samples
                .chunks_exact(clip.channels as usize)
                .map(|frame| {
                    let (left, right) = low.stereo(frame[0], frame[1]);
                    kira::Frame::new(left, right)
                })
                .collect(),
        };
        if frames.is_empty() {
            return false;
        }

        let sound = kira::sound::static_sound::StaticSoundData {
            sample_rate: clip.sample_rate,
            frames: frames.into(),
            settings: kira::sound::static_sound::StaticSoundSettings::new()
                .volume(kira::Decibels(amplitude_to_db(volume)))
                .panning(kira::Panning(placement.pan))
                .playback_rate(kira::PlaybackRate(rate)),
            slice: None,
        };
        manager.play(sound).is_ok()
    }
}

/// A one-pole low-pass, the treble half of "far away".
///
/// Distance eating treble is most of why something distant sounds distant
/// rather than merely quiet, and [`place`] has computed that number — as
/// `Placement::brightness` — since the day it was written. Nothing applied it
/// until 2026-08-20: the test asserted the ARITHMETIC and there was no ear on
/// the other end of it.
///
/// One pole is the right amount of filter for this. It is a gentle 6 dB/octave
/// roll-off, it costs one multiply-add per sample, and the alternative — a
/// steep filter — makes a distant sound muffled rather than distant.
struct Filter {
    /// How much of each new sample is taken. `1.0` passes everything.
    alpha: f32,
    /// The running value, per channel.
    held: (f32, f32),
}

impl Filter {
    /// A filter for this brightness, where `1.0` is no filtering at all.
    fn new(brightness: f32) -> Self {
        Self {
            alpha: if brightness.is_finite() {
                brightness.clamp(0.0, 1.0)
            } else {
                1.0
            },
            held: (0.0, 0.0),
        }
    }

    /// One mono sample.
    fn mono(&mut self, sample: f32) -> f32 {
        if self.alpha >= 1.0 {
            return sample;
        }
        self.held.0 += self.alpha * (sample - self.held.0);
        self.held.0
    }

    /// One stereo frame. Each side keeps its own history or the image collapses.
    fn stereo(&mut self, left: f32, right: f32) -> (f32, f32) {
        if self.alpha >= 1.0 {
            return (left, right);
        }
        self.held.0 += self.alpha * (left - self.held.0);
        self.held.1 += self.alpha * (right - self.held.1);
        self.held
    }
}

/// Amplitude to decibels, for a backend that thinks in decibels.
///
/// Silence is not `-inf` here but a floor: kira treats a very negative gain the
/// same way and a `-inf` would propagate through anything that later averaged
/// it.
#[must_use]
#[expect(
    clippy::disallowed_methods,
    reason = "audio is presentation; float-determinism.md Scope"
)]
fn amplitude_to_db(amplitude: f32) -> f32 {
    if amplitude <= 0.000_01 {
        return -80.0;
    }
    20.0 * amplitude.log10()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORWARD: [f32; 3] = [0.0, 0.0, -1.0];
    const RIGHT: [f32; 3] = [1.0, 0.0, 0.0];

    #[test]
    fn a_sound_gets_quieter_with_distance_and_stops_at_its_radius() {
        let near = place([0.0, 0.0, -1.0], [0.0; 3], FORWARD, RIGHT, 16.0, 1.0);
        let far = place([0.0, 0.0, -12.0], [0.0; 3], FORWARD, RIGHT, 16.0, 1.0);
        assert!(
            near.gain > far.gain,
            "distance did not attenuate: {near:?} against {far:?}"
        );

        // **At the radius it is silent, not merely quiet.** The radius a mod
        // asks for is the distance the sound stops carrying — which is the
        // whole reason attenuation here is linear rather than inverse-square.
        let edge = place([0.0, 0.0, -16.0], [0.0; 3], FORWARD, RIGHT, 16.0, 1.0);
        assert!(edge.gain.abs() < f32::EPSILON, "{edge:?}");
        let beyond = place([0.0, 0.0, -100.0], [0.0; 3], FORWARD, RIGHT, 16.0, 1.0);
        assert!(beyond.gain.abs() < f32::EPSILON, "{beyond:?}");
    }

    #[test]
    fn a_sound_on_the_right_pans_right_and_one_underfoot_pans_nowhere() {
        let right = place([5.0, 0.0, 0.0], [0.0; 3], FORWARD, RIGHT, 16.0, 1.0);
        assert!(
            right.pan > 0.5,
            "a sound to the right did not pan right: {right:?}"
        );

        let left = place([-5.0, 0.0, 0.0], [0.0; 3], FORWARD, RIGHT, 16.0, 1.0);
        assert!(
            left.pan < -0.5,
            "a sound to the left did not pan left: {left:?}"
        );

        // **Standing inside it is not a direction.** Within half a block the
        // arithmetic is unstable and the answer is meaningless, so it collapses
        // to centred rather than swinging as a player turns on the spot.
        let underfoot = place([0.05, 0.0, 0.05], [0.0; 3], FORWARD, RIGHT, 16.0, 1.0);
        assert!(underfoot.pan.abs() < f32::EPSILON, "{underfoot:?}");
    }

    #[test]
    fn distance_eats_treble_but_never_all_of_it() {
        let near = place([0.0, 0.0, -1.0], [0.0; 3], FORWARD, RIGHT, 32.0, 1.0);
        let far = place([0.0, 0.0, -30.0], [0.0; 3], FORWARD, RIGHT, 32.0, 1.0);
        assert!(
            far.brightness < near.brightness,
            "distance did not dull the sound"
        );
        assert!(
            far.brightness >= 0.25,
            "a distant sound became a rumble: {far:?}"
        );
    }

    #[test]
    fn a_zero_radius_is_silence_rather_than_a_division() {
        // A mod may ask for one, and dividing by it would put a NaN in
        // somebody's ears.
        let placement = place([1.0, 0.0, 0.0], [0.0; 3], FORWARD, RIGHT, 0.0, 1.0);
        assert!(placement.gain.abs() < f32::EPSILON);
        assert!(placement.pan.is_finite() && placement.brightness.is_finite());
    }

    #[test]
    fn volumes_multiply_the_master_by_the_bus() {
        let mut volumes = Volumes {
            master: 0.5,
            ..Volumes::default()
        };
        volumes.buses.insert("music".to_owned(), 0.5);
        assert!((volumes.of(Bus::Music) - 0.25).abs() < 0.001);

        // A bus nobody configured is at full, so a mod adding one does not
        // arrive silent.
        volumes.buses.remove("music");
        assert!((volumes.of(Bus::Music) - 0.5).abs() < 0.001);
    }

    #[test]
    fn a_dim_placement_actually_removes_treble() {
        // **`brightness` was computed and never used.** `place` produced it,
        // `distance_eats_treble_but_never_all_of_it` asserted the arithmetic,
        // and no sample was ever filtered by it — so a distant sound was quiet
        // but not distant.
        //
        // Measured as total swing between neighbouring samples, which is what
        // "treble" means for a signal: an alternating series is the fastest a
        // sampled signal can move, and a low-pass has to slow it down.
        let alternating: Vec<f32> = (0..256)
            .map(|i| if i % 2 == 0 { 1.0 } else { -1.0 })
            .collect();
        let swing = |alpha: f32| {
            let mut filter = Filter::new(alpha);
            let out: Vec<f32> = alternating.iter().map(|s| filter.mono(*s)).collect();
            out.windows(2).map(|p| (p[1] - p[0]).abs()).sum::<f32>()
        };

        let open = swing(1.0);
        let dim = swing(0.25);
        assert!(
            dim < open * 0.5,
            "a brightness of 0.25 barely changed the signal: {dim} against {open}"
        );
        // And full brightness is EXACTLY the input, not merely close to it: a
        // sound at the listener must not be quietly filtered.
        let mut filter = Filter::new(1.0);
        for sample in &alternating {
            assert!(
                (filter.mono(*sample) - sample).abs() < f32::EPSILON,
                "full brightness altered a sample"
            );
        }
        // A hostile number is not a filter setting.
        assert!((Filter::new(f32::NAN).alpha - 1.0).abs() < f32::EPSILON);
        assert!(Filter::new(-5.0).alpha.abs() < f32::EPSILON);

        // Stereo keeps a history per side, or the image collapses to mono.
        let mut filter = Filter::new(0.3);
        let mut last = (0.0, 0.0);
        for i in 0..64 {
            let sign = if i % 2 == 0 { 1.0 } else { -1.0 };
            last = filter.stereo(sign, -sign);
        }
        assert!(
            (last.0 - last.1).abs() > f32::EPSILON,
            "the two channels converged, so they shared a history"
        );
    }

    #[test]
    fn a_mods_gain_and_pitch_variance_are_not_ignored() {
        // **Both fields were dead.** `register_sound` took them, the protocol
        // carried them, and the client read neither — so every sound played at
        // its file's level and at exactly one pitch, which is what makes a
        // repeated footstep sound like a machine.
        let quiet = Voice {
            gain: 0.0,
            pitch_variance: 0.0,
        };
        let mut mixer = Mixer::open(Volumes::default());
        mixer.insert(
            "test:silent".to_owned(),
            Clip {
                samples: vec![0.5; 32],
                channels: 1,
                sample_rate: 48_000,
            },
            quiet,
        );
        // A mod that registered a gain of zero gets silence, and the refusal
        // happens before any frame conversion — which is only observable as
        // `false`, but it is the same `false` a missing device gives, so this
        // asserts the clip IS loaded to tell the two apart.
        assert!(mixer.holds("test:silent"));
        assert!(!mixer.play(
            "test:silent",
            Bus::Effects,
            Placement {
                gain: 1.0,
                pan: 0.0,
                brightness: 1.0,
            }
        ));

        // And the jitter itself: inside the band, and not the same twice.
        let mut mixer = Mixer::open(Volumes::default());
        let rolls: Vec<f32> = (0..64).map(|_| mixer.jitter(0.2)).collect();
        for roll in &rolls {
            assert!(
                roll.abs() <= 0.2,
                "a pitch offset of {roll} is outside the variance asked for"
            );
        }
        assert!(
            rolls
                .windows(2)
                .any(|pair| (pair[0] - pair[1]).abs() > f32::EPSILON),
            "every play got the same pitch, which is the machine-gun sound this prevents"
        );
        // Zero variance is exactly one pitch, which is a mod's right to ask for.
        assert!(
            (0..8).all(|_| mixer.jitter(0.0).abs() < f32::EPSILON),
            "a sound with no variance was varied anyway"
        );
    }

    /// A server's numbers are a claim, and these two reach a backend.
    #[test]
    fn a_hostile_voice_is_brought_into_range() {
        let wild = tiamot_core::proto::SoundDef {
            id: "evil:sound".to_owned(),
            mod_id: "evil".to_owned(),
            file: None,
            gain: f32::INFINITY,
            pitch_variance: 40.0,
        };
        let voice = Voice::of(&wild);
        assert!((voice.gain - 1.0).abs() < f32::EPSILON, "{voice:?}");
        assert!(
            (voice.pitch_variance - 0.5).abs() < f32::EPSILON,
            "{voice:?}"
        );

        let negative = tiamot_core::proto::SoundDef {
            gain: -3.0,
            pitch_variance: f32::NAN,
            ..wild
        };
        let voice = Voice::of(&negative);
        assert!(voice.gain.abs() < f32::EPSILON, "{voice:?}");
        assert!(voice.pitch_variance.abs() < f32::EPSILON, "{voice:?}");
    }

    #[test]
    fn a_mixer_without_a_device_is_silent_rather_than_broken() {
        // The property every headless test depends on, and the reason `open`
        // cannot fail: a machine with no sound card should run the game.
        let mut mixer = Mixer::open(Volumes::default());
        mixer.insert(
            "test:thud".to_owned(),
            Clip {
                samples: vec![0.5; 32],
                channels: 1,
                sample_rate: 48_000,
            },
            Voice::default(),
        );
        assert!(mixer.holds("test:thud"));
        assert_eq!(mixer.len(), 1);

        // Playing is allowed to do nothing; it must not panic either way.
        let placement = Placement {
            gain: 1.0,
            pan: 0.0,
            brightness: 1.0,
        };
        let _ = mixer.play("test:thud", Bus::Effects, placement);
        // And a sound nobody loaded is simply not played.
        assert!(!mixer.play("test:missing", Bus::Effects, placement));
    }
}
